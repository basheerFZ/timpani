# eBPF Programs for timpani-n

This directory contains eBPF (extended Berkeley Packet Filter) programs that run in kernel space for real-time monitoring.

## Files

### Kernel-Space eBPF Programs (C code - DO NOT MODIFY without understanding eBPF constraints)

- **schedstat.bpf.c**: Monitors scheduler events (sched_waking, sched_switch tracepoints)
  - Tracks task wakeup times, execution start/stop times
  - Measures scheduling latency and execution time
  - Used for deadline miss detection

- **sigwait.bpf.c**: Monitors signal delivery timing
  - Tracks rt_sigtimedwait syscall entry/exit
  - Measures signal delivery latency
  - Critical for time-triggered task activation

- **trace_bpf.h**: Common data structures shared between kernel and userspace
  - `struct schedstat_event`: Scheduler statistics event
  - `struct sigwait_event`: Signal wait event

### Architecture-Specific Headers

- **aarch64/vmlinux.h**: ARM64 kernel type definitions (BTF)
- **x86_64/vmlinux.h**: x86-64 kernel type definitions (BTF)

These files are generated using `bpftool btf dump file /sys/kernel/btf/vmlinux format c > vmlinux.h`

## Building

The eBPF programs are compiled by the build system (build.rs) using:
- clang with BPF target
- libbpf headers
- BTF (BPF Type Format) for CO-RE (Compile Once - Run Everywhere)

## Userspace Integration

The Rust userspace code uses `libbpf-rs` to:
1. Load these eBPF programs into the kernel
2. Attach them to tracepoints/syscalls
3. Read events from ring buffers
4. Process scheduler statistics for deadline checking

## Important Notes

⚠️ **These are kernel-space programs with restrictions:**
- No unbounded loops
- Limited stack size (512 bytes)
- No dynamic memory allocation
- Must pass BPF verifier checks
- Requires GPL license

✅ **Advantages:**
- Zero userspace↔kernel context switches for monitoring
- Nanosecond-precision timing
- No events missed
- Minimal overhead on real-time tasks
