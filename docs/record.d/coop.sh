for k in coop overflow disk memmap pool fit frag tlsf slab pages smp lisp shard gc smr gen incremental safety; do
    kernel $k riscv64 $k-riscv64.txt
done
kernel disk x86_64 disk-x86_64.txt
