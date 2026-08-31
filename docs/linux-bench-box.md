# Provisioning a benchmark box — and why you probably should not

**Read this first: the conclusion changed.**

This document was written to specify a dedicated machine, on the premise that
the per-stage latency budget could not be resolved without core isolation. Five
of six stages fell under their own noise floor on the development laptop, and
the diagnosis was that macOS gives no CPU pinning and the M2 mixes performance
and efficiency cores.

That diagnosis was wrong, or at least premature. **The dominant noise source was
experimental design, not hardware.** Two changes fixed it:

* a **shorter corpus** (60k segments rather than 2M packets), so a single
  repetition finishes before machine load has time to drift; and
* **round-robin repetitions** across stages rather than timing each stage in a
  block, so any drift that remains is shared evenly instead of accumulating
  along the stage order.

With those, the free shared-CPU VM already serving the demo resolves five of six
stages with spreads of 2.0–7.1 ns:

```
  stage                        cumulative   this stage   spread
  receive (eth/vlan/ip/udp)       10.2 ns      10.2 ns    2.0 ns
  sequence (IEX-TP, A/B)          19.0 ns       8.8 ns    2.0 ns
  parse (DEEP)                    45.7 ns      26.7 ns    4.4 ns
  book update                    125.1 ns      79.4 ns    7.1 ns
  decide (strategy)              130.4 ns       5.2 ns    3.1 ns
  encode (OUCH order)            143.3 ns      13.0 ns   20.7 ns  <- unresolved
```

Live at `/budget` on the deployed host. **A dedicated machine is not required for
this, and renting one monthly would have been paying to avoid fixing a
methodology problem.**

## What a rented box would still buy

Three things, none of which justify a monthly commitment:

1. **`perf`.** macOS has no equivalent. Cache-miss counters would confirm or
   refute the inference about why the arena container lost, which is currently
   reasoning rather than measurement.
2. **AF_XDP (R2).** Kernel bypass genuinely needs Linux, a recent kernel, and
   control of boot parameters.
3. **The last stage.** `encode` is still unresolved at 13.0 ns against a 20.7 ns
   spread; real core isolation would settle it.

## If you do want one: rent it by the hour, not the month

A benchmark is a measurement, not a service. Take the numbers, commit them,
destroy the machine.

| option | cost for one session | gives you |
|---|---|---|
| **Fly `performance-2x`**, scaled up temporarily | pennies — billed per second | dedicated vCPU on infrastructure already set up |
| **Hetzner CCX13** (dedicated vCPU, hourly) | ~€0.30 for five hours | dedicated cores, full root, kernel boot params |
| **AWS `c6i.metal`** | ~$11 for two hours | true bare metal, full isolation, `perf` |

Scaling the existing Fly machine is the cheapest first move by a wide margin,
because the deployment already exists:

```bash
flyctl scale vm performance-2x --app nanobook   # run the benchmarks
flyctl scale vm shared-cpu-1x  --app nanobook   # scale back down
```

**Do not rent anything monthly for this.** The measurements are one-off.

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
git clone https://github.com/pranitmathur06/NanoBook && cd NanoBook
# Fetch a capture; see the README. Do not commit it.
taskset -c 2 cargo run --release --example budget   -- data/raw/<file>.pcap
taskset -c 2,3 cargo run --release --example threaded -- data/raw/<file>.pcap
```

`taskset` onto isolated cores is the point of all of the above. Re-run
`cargo test` there first — the SPSC queue has only ever been exercised on a
weakly-ordered machine, and x86 is where a *missing* barrier would pass silently,
so agreement between the two is worth confirming rather than assuming.

## What to re-measure if you do rent one

1. ~~The stage budget.~~ **Resolved without one** — see the top of this file.
2. **The threaded comparison.** Still indistinguishable, with a 167 ns spread.
   The same shorter-corpus and round-robin fixes should be tried there first,
   before assuming hardware is the answer; that assumption was wrong once here
   already.
3. **The container bake-off.** `perf stat` can confirm or refute the cache-miss
   explanation for why the arena lost, which is presently an inference.
4. ~~ARM vs x86.~~ **Already covered**: the M2 develops it, CI runs it on x86,
   and the deployed host measures it on x86.
