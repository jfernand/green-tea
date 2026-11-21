for a in riscv64 aarch64 x86_64; do kernel memmap $a memmap-$a.txt; done
kernel overflow riscv64 overflow-riscv64.txt
kernel overflow x86_64 overflow-x86_64.txt
kernel pages riscv64 pages-riscv64.txt
kernel smp x86_64 smp-x86_64.txt
