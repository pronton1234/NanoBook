# Provisioning the benchmark box

Everything measured on the M2 so far carries the same caveat: macOS gives no CPU
pinning, no CPU isolation, and no control over which cores a thread lands on. The
consequences are visible in the numbers — the stage budget's per-stage split sits
below its own noise floor, and the threaded comparison's run-to-run spread
(167 ns) is larger than the whole pipeline it is measuring.

This is what to build so those numbers resolve, and what R2 (kernel bypass)
needs.

## What the box has to provide

| requirement | why |
|---|---|
| **x86_64** | what trading systems actually run; also lets the ARM/x86 delta be reported |
| **root, and control of kernel boot parameters** | `isolcpus` / `nohz_full` cannot be set any other way |
| **kernel ≥ 5.10** | AF_XDP; 5.4 is the floor, later kernels have far better driver coverage |
| **frequency control** | a core that boosts and throttles produces the spread we already have |
| **bare metal, or a full VM with dedicated cores** | shared/burstable vCPUs reintroduce exactly the jitter this removes |

## Do NOT plan on two cloud instances talking multicast

The obvious design for R2 is one instance running `tcpreplay` and another
receiving via AF_XDP. **Cloud VPCs generally do not forward multicast, and they
filter frames whose source MAC or IP does not match the interface.** IEX captures
are multicast, so a replay onto a cloud VPC is likely to be dropped before it
reaches the receiver — and the failure looks like a bug in the receiver.

Use **one box with a veth pair in network namespaces** instead. `veth` has
supported native XDP since kernel 4.14, so the full path — real driver, real
kernel network stack, real AF_XDP socket — is exercised without a provider's
network policy in the way. It is also one machine instead of two.

Verify this claim before building on it; it is from documentation, not from a
box I have run.

## Steps

### 1. Rent the machine

Either works; the trade is cost against fidelity.

- **Dedicated server** (e.g. Hetzner AX-series, ~€40–60/month). Real hardware,
  full boot control, cheapest if benchmarking spans weeks.
- **AWS `c6i.metal` / `m6i.metal`** (~$5/hr). Bare metal, hourly, right when
  benchmarking is a few concentrated sessions. Stop it between runs.

Avoid burstable (`t3`, `t4g`) and anything with shared vCPUs.

Install **Ubuntu 24.04 LTS**.

### 2. Toolchain

```bash
sudo apt update && sudo apt install -y \
  build-essential clang llvm pkg-config \
  libbpf-dev libxdp-dev libelf-dev zlib1g-dev \
  linux-tools-common linux-tools-$(uname -r) \
  tcpreplay iproute2 ethtool numactl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
```

`linux-tools-$(uname -r)` is `perf`, which is the tool macOS has no equivalent of
and the reason several questions in this project are currently unanswerable.

### 3. Isolate cores

Edit `/etc/default/grub`, appending to `GRUB_CMDLINE_LINUX_DEFAULT` (adjust the
core list to the machine — leave core 0 for the OS):

```
isolcpus=2-7 nohz_full=2-7 rcu_nocbs=2-7 intel_pstate=disable processor.max_cstate=1 idle=poll
```

- `isolcpus` — the scheduler stops placing work on those cores
- `nohz_full` — stops the periodic timer tick on them
- `rcu_nocbs` — moves RCU callbacks off them
- `max_cstate=1` / `idle=poll` — stops deep C-states, whose wake-up latency shows
  up directly in a p99.9

Then `sudo update-grub && sudo reboot`.

### 4. Pin frequency and disable SMT

```bash
# Fixed frequency: a boosting core makes every repetition a different machine.
sudo cpupower frequency-set -g performance
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo

# SMT off: a sibling thread shares L1 and steals the cache being measured.
echo off | sudo tee /sys/devices/system/cpu/smt/control
```

### 5. Hugepages

```bash
echo 1024 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
```

Reduces TLB misses across the ~1.3 MB symbol table and the level storage — one of
the few remaining suspects for the book stage's 51.6 ns.

### 6. The veth pair for R2

```bash
sudo ip netns add sender
sudo ip link add veth0 type veth peer name veth1
sudo ip link set veth1 netns sender
sudo ip addr add 10.0.0.1/24 dev veth0 && sudo ip link set veth0 up
sudo ip netns exec sender ip addr add 10.0.0.2/24 dev veth1
sudo ip netns exec sender ip link set veth1 up
```

Then replay a capture from the sender namespace and receive on `veth0`, once with
an ordinary UDP socket and once with AF_XDP. The delta between those two is R2.

### 7. Run

```bash
git clone https://github.com/pronton1234/LatencyLadder && cd LatencyLadder
# Fetch a capture; see the README. Do not commit it.
taskset -c 2 cargo run --release --example budget   -- data/raw/<file>.pcap
taskset -c 2,3 cargo run --release --example threaded -- data/raw/<file>.pcap
```

`taskset` onto isolated cores is the point of all of the above. Re-run
`cargo test` there first — the SPSC queue has only ever been exercised on a
weakly-ordered machine, and x86 is where a *missing* barrier would pass silently,
so agreement between the two is worth confirming rather than assuming.

## What to re-measure once it exists

1. **The stage budget.** Five of six stage costs currently sit below the noise
   floor and are not quotable.
2. **The threaded comparison.** Currently indistinguishable with a 167 ns spread;
   on pinned cores the split should resolve one way or the other.
3. **The container bake-off.** `perf stat` can confirm or refute the cache-miss
   explanation for why the arena lost, which is presently an inference.
4. **ARM vs x86.** Same binary, same capture, both architectures — the comparison
   is itself a result.
