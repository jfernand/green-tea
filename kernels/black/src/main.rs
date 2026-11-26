#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, Ordering};
use green_tea_bare::{
    Board, Platform, ProcessOutcome, USER_REGION_A, USER_REGION_B, UserProcess, boot, memory,
    println,
};
use green_tea_scheduler_coop::runtime::{run, spawn};

static WITNESS_RAN: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    let _framebuffer = boot("green-tea-black");
    run(
        || {
            spawn(process_tests);
            spawn(|| {
                WITNESS_RAN.store(true, Ordering::Release);
                println!("kernel witness task continued after user faults");
            });
        },
        |_| {},
    );
    assert!(
        WITNESS_RAN.load(Ordering::Acquire),
        "the kernel witness task did not run"
    );
    println!("PASS: user exit, syscall checks, kernel/port/page isolation and fault cleanup");
    Board::halt()
}

fn process_tests() {
    println!("user process: valid exit");
    let exit = exit_program(42);
    let mut process = UserProcess::new(&exit, USER_REGION_A).expect("create user process");
    assert_eq!(process.run(), ProcessOutcome::Exited(42));

    println!("user process: syscall validation");
    let invalid_syscall = syscall_error_program(3);
    let mut process =
        UserProcess::new(&invalid_syscall, USER_REGION_A).expect("create syscall test process");
    assert_eq!(process.run(), ProcessOutcome::Exited(0));

    println!("user process: kernel mapping denial");
    let kernel_address = memory::sections()[0].start;
    let read_kernel = read_program(kernel_address);
    let mut process =
        UserProcess::new(&read_kernel, USER_REGION_A).expect("create kernel probe process");
    assert_eq!(
        process.run(),
        ProcessOutcome::Fault {
            vector: 14,
            error: process_fault_error(1),
            address: kernel_address
        },
    );

    println!("user process: port I/O denial");
    let port_access = port_program();
    let mut process =
        UserProcess::new(&port_access, USER_REGION_A).expect("create port probe process");
    assert!(matches!(
        process.run(),
        ProcessOutcome::Fault { vector: 13, .. }
    ));

    println!("user process: sibling mapping denial");
    let sibling_code = exit_program(0);
    let sibling = UserProcess::new(&sibling_code, USER_REGION_B).expect("create sibling process");
    let sibling_page = sibling.code_address();
    let read_sibling = read_program(sibling_page);
    let mut process =
        UserProcess::new(&read_sibling, USER_REGION_A).expect("create isolation probe process");
    assert_eq!(
        process.run(),
        ProcessOutcome::Fault {
            vector: 14,
            error: process_fault_error(0),
            address: sibling_page
        },
    );
    let mut sibling = sibling;
    assert_eq!(sibling.run(), ProcessOutcome::Exited(0));

    #[cfg(feature = "board-pc")]
    {
        let lease = Board::claim_user_i8042().expect("claim the i8042 input broker");
        println!("user keyboard: controller lease acquired");
        let ungranted = syscall_error_program(2);
        let mut process =
            UserProcess::new(&ungranted, USER_REGION_A).expect("create ungranted process");
        assert_eq!(process.run(), ProcessOutcome::Exited(0));
        drop(process);
        println!("user keyboard: ungranted process denied");

        println!("user keyboard: waiting for A scan code");
        let keyboard = get_key_program();
        let mut process = UserProcess::with_i8042(&keyboard, USER_REGION_A, &lease)
            .expect("create authorized keyboard client process");
        assert_eq!(process.run(), ProcessOutcome::Exited(u32::from(b'a')));
        drop(process);
        println!(
            "user keyboard: decoded A, dropped scan codes {}",
            lease.dropped_scan_codes()
        );

        let failing_driver = port_program();
        let mut process = UserProcess::with_i8042(&failing_driver, USER_REGION_A, &lease)
            .expect("create faulting driver process");
        assert!(matches!(
            process.run(),
            ProcessOutcome::Fault { vector: 13, .. }
        ));
        drop(process);
        println!("user driver fault contained; releasing controller lease");
        drop(lease);

        println!("kernel keyboard: waiting for B");
        loop {
            if Board::getc() == Some(b'b') {
                break;
            }
            Board::wait_for_interrupt();
        }
        println!("kernel keyboard fallback delivered B after lease cleanup");
    }
    println!("protected process checks completed; kernel task resumed after contained faults");
}

fn exit_program(code: u32) -> [u8; 14] {
    let mut program = [0u8; 14];
    program[..5].copy_from_slice(&[0xb8, 1, 0, 0, 0]); // mov eax, EXIT
    program[5..10].copy_from_slice(&[
        0xbf,
        code as u8,
        (code >> 8) as u8,
        (code >> 16) as u8,
        (code >> 24) as u8,
    ]);
    program[10..12].copy_from_slice(&[0xcd, 0x80]); // int 0x80
    program[12..14].copy_from_slice(&[0x0f, 0x0b]); // ud2 if exit returned
    program
}

fn syscall_error_program(number: u32) -> [u8; 41] {
    let mut program = [0u8; 41];
    program[..5].copy_from_slice(&[0xb8, number as u8, 0, 0, 0]);
    program[5..7].copy_from_slice(&[0xcd, 0x80]);
    program[7..11].copy_from_slice(&[0x48, 0x83, 0xf8, 0xff]); // cmp rax, -1
    program[11..13].copy_from_slice(&[0x75, 14]); // jne failure
    program[13..27].copy_from_slice(&exit_program(0));
    program[27..41].copy_from_slice(&exit_program(1));
    program
}

fn read_program(address: usize) -> [u8; 24] {
    let mut program = [0u8; 24];
    program[..2].copy_from_slice(&[0x48, 0xa1]); // mov rax, moffs64
    program[2..10].copy_from_slice(&(address as u64).to_le_bytes());
    program[10..24].copy_from_slice(&exit_program(0));
    program
}

fn port_program() -> [u8; 20] {
    let mut program = [0u8; 20];
    program[..6].copy_from_slice(&[0xba, 0x60, 0, 0, 0, 0xee]); // mov edx, 0x60; out dx, al
    program[6..20].copy_from_slice(&exit_program(0));
    program
}

#[cfg(feature = "board-pc")]
fn get_key_program() -> [u8; 50] {
    let mut program = [0u8; 50];
    program[..5].copy_from_slice(&[0xb8, 2, 0, 0, 0]); // GET_SCAN_CODE syscall
    program[5..10].copy_from_slice(&[0xbf, 0xb8, 0x0b, 0, 0]); // mov edi, 3000 ms timeout
    program[10..12].copy_from_slice(&[0xcd, 0x80]);
    program[12..14].copy_from_slice(&[0x3c, 0x1e]); // cmp al, set-1 'A' make code
    program[14..16].copy_from_slice(&[0x74, 0x06]); // je success
    program[16..18].copy_from_slice(&[0x3c, 0xff]); // cmp al, timeout/denied sentinel
    program[18..20].copy_from_slice(&[0x74, 0x10]); // je failure
    program[20..22].copy_from_slice(&[0xeb, 0xea]); // retry
    program[22..27].copy_from_slice(&[0xbf, b'a', 0, 0, 0]); // success: exit status 'a'
    program[27..32].copy_from_slice(&[0xb8, 1, 0, 0, 0]); // EXIT syscall
    program[32..34].copy_from_slice(&[0xcd, 0x80]);
    program[34..36].copy_from_slice(&[0x0f, 0x0b]);
    program[36..41].copy_from_slice(&[0xbf, 1, 0, 0, 0]); // failure: exit status 1
    program[41..46].copy_from_slice(&[0xb8, 1, 0, 0, 0]);
    program[46..48].copy_from_slice(&[0xcd, 0x80]);
    program[48..50].copy_from_slice(&[0x0f, 0x0b]);
    program
}

const fn process_fault_error(present: u64) -> u64 {
    present | (1 << 2) // user-mode access
}
