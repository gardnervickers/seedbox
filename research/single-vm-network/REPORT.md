# Single-VM packet networking qualification

**Qualified on the merged-main ABI1 backend, not the optimized PV/undo backend.** At identical 1 GiB-per-guest settings, fixed 128 successful writes took **27.310 seconds with four guests versus 4.830 seconds with one guest**, a **5.654× end-to-end ratio**. Both use the same main VMM, new compatible guest kernel, etcd/client binaries and networking-tool closure. This ratio is specific to these inputs and is neither a speedup over the performance branch nor directly comparable with the earlier loopback/PV 3.2× result.

**Decision: pursue this as a complementary backend.** Real network namespaces, routed packets, fixed delay and bidirectional node isolation survive exact replay in this bounded fixture. Retain multi-VM tests for independent kernel/reboot, clocks and resource failure. Bug-finding rate per host-hour and compatibility with the owner's PV/undo integration are later gates; no such result is claimed here.

Issue: [#10](https://github.com/gardnervickers/seedbox/issues/10). Source baseline is `fa51542b192741aa71d21bac98f37f743172996c`; the separately owned architecture memo commit `ae732a3` changes no simulator code. All implementation is under this directory. The old MVP and evidence were preserved.

## Packet path and scope

Each of n1, n2, n3 and the client (n4) has a separate Linux network namespace, its own loopback/interface/routing table and socket namespace. Namespace n<i> has `10.0.<i>.1/30`; the root namespace router has `10.0.<i>.2` on host<i>, joined by a veth pair. Root IPv4 forwarding connects the four networks. All service/client sockets use the explicit service addresses. There is no TCP proxy.

Fixed `netem delay 1ms` is applied only on root-to-service veth egress. A packet crossing namespaces therefore encounters **one** delayed qdisc, not two; an echoed round trip encounters two. No stochastic jitter, loss, corruption or reordering is configured. The qdisc initializes and reports a seeded RNG state, but this experiment qualifies only the fixed-delay/100%-drop behavior under the pinned guest and duplicate-run checks.

`partition n1` replaces root-to-n1 egress with 100% drop and installs an ingress matchall/drop action for n1-to-root traffic. It cuts n1 in both directions against all peers/client. n2↔n3 remains connected. `heal n1` removes the ingress filter and restores delay-only egress. This is per-node isolation, not arbitrary pair-specific directed partitions. It acts on packets of existing sockets/connections; it does not rewrite service addresses or disconnect a proxy.

Only network isolation is implemented. PID, mount, user and cgroup isolation are not provided; all processes share one kernel, memory and clock domain, root privileges and a tmpfs data mount. The serial helper also retains SIGKILL/restart of individual etcd processes against their existing data directories.

## Acceptance and measurements

The final profile uses seed 1009, two client workers, 64 successful writes per worker, a 1.5-second guest client-start delay, a 10-second virtual load deadline and a 20-second virtual world horizon. Both layouts use 1-ms configured network delay, zero jitter, zero timer jitter and 2-ms stop tolerance. There are two fresh-process runs per healthy layout, two packet-control runs and two fault-control runs. The checker-negative control is one sample.

Acceptance requires process status 0, `end_machine_exited`, the designated guest state `Exited(0)` (or `Exited(1)` for the checker-negative control) and the appropriate semantic output. A VMM returning 0 is insufficient: it can stop at the world horizon while the guest checker is still waiting. Replay grouping uses the exact captured input manifest and generated world configuration. Mixed input identities cannot silently contribute to one comparison median.

| Final 1 GiB profile | Four guests | One guest with namespaces |
| --- | ---: | ---: |
| Successful writes per run | 128, zero failed | 128, zero failed |
| Command wall seconds, two runs | 27.310015, 27.309544 | 4.830344, 4.830390 |
| Median command wall seconds | 27.309780 | 4.830367 |
| Median VMM execution-loop seconds | 24.378841 | 3.979798 |
| Median VMM exits | 3,684,230 | 557,093 |
| Total retired branches | 429,852,349 | 234,002,537 |
| Configured aggregate guest RAM | 4 × 1 GiB | 1 × 1 GiB |

The command-wall ratio is 5.653769×; the execution-loop ratio is about 6.126×. Subtracting execution-loop time from command time gives about 2.931 versus 0.851 seconds. That remainder combines initialization/image loading, final RAM hashing, process startup and output; it is not isolated guest boot time. The larger four-guest RAM footprint increases initialization/digest cost. Both initramfs images are about 122 MiB and carry the same 86-MiB dynamic iproute2 runtime closure, even though only one layout needs it for namespaces.

The VMM fabric and guest netem have different scheduling semantics. Main's `deliver_due` permits a 0.5-ms early event window with this configuration (`min(latency/2, stop_tolerance)`). The configured multi-VM 1 ms therefore does not establish exactly 1 ms elapsed delivery. Guest netem releases packets through Linux timers; receiver-side veth egress is close to receiver ingress but does not make the two backends timing-equivalent. These results compare equal successful work under stated network parameters and useful packet isolation; they do not prove identical-fidelity throughput.

## Exact replay evidence

For each of the final packet-controls, healthy multi-VM, healthy single-VM and fault fixtures, two runs matched **all four** independently checked values: world witness, every per-machine event witness, every final RAM/register digest, and per-machine console SHA256. The VMM folds final state into its world witness when `--digest` is used; checking the separate fields avoids mistaking that combined hash for an independent event-only check.

| Fixture | Repeats | Repeated world witness |
| --- | ---: | --- |
| Packet controls | 2 | `2362eff1a0ed1228f254a549e3e93838727adadc0b7df29cd35818c2307247b6` |
| Four-VM healthy workload | 2 | `ecabb8ffb5e4af40dd02d045ef438dcb454ad9ba4d76a5977d8980d9940eb991` |
| One-VM healthy workload | 2 | `18a066be8321494a0324856337be828dda23e6a778fec7fde8a2fbb54cf34763` |
| One-VM network/process faults | 2 | `ade8e03f792224c2e43fcaaa45bbf568ad2a7c4cad9537f4c778be69549dd976` |

This is replay evidence for the chosen seed, pinned inputs and small fixtures, not a universal determinism proof. Cross-backend witnesses, revision ordering and HashKV values are not required to match. Within each run the original checker verifies every acknowledged value and all members' revision/hash consistency.

## Control results

The namespace control reports distinct network namespace inodes and rejects binding n2's address from n1 with `EADDRNOTAVAIL` (errno 99). UDP servers bind the same wildcard port in separate namespaces. All four destination qdiscs read back delay 1 ms.

After ARP warmup, three zero-delay round trips read as 6 ns each. Three fixed-delay round trips read as **2,427,895; 2,433,235; 2,434,888 ns**, exactly repeated. The analyzer requires a median increase of at least 1.5 ms, as well as the 2-ms per-probe floor and qdisc readback. The 6-ns baseline is an artifact of the inherited ABI1 virtual-time publication: repeated kernel TSC reads with no fresh VMM publication advance by one tick. It does not measure physical transmission latency or precise syscall cost. The injected queue/timer interval is nevertheless observable and repeatable. [Netem documentation](https://man7.org/linux/man-pages/man8/tc-netem.8.html) also identifies timer granularity as a limit on delay behavior; this fixture measures its own timer behavior rather than assuming the configured value proves delivery timing.

During n1 isolation, both n1→n2 and n2→n1 probes time out, an n2→n3 probe succeeds, and both formerly blocked directions succeed after heal. No probe reports `pass=0`. [Network namespace documentation](https://man7.org/linux/man-pages/man7/network_namespaces.7.html) describes the interface, routing and socket resources isolated by these namespaces; the broader shared-kernel limitations remain.

The real-service fault scenario schedules partition/heal requests for n1 at 2.2/2.9 virtual seconds and stop/start requests for n2 at 3.3/3.9. Those are scenario request times; the inherited early-delivery window and guest helper processing affect actual execution timing. Both runs record all four management acknowledgements and the same **142 acknowledged writes, three failed requests, revision 144 and final checker pass**. The timed load lasts 3.806 virtual seconds because requests in flight may finish after its three-second deadline. This establishes packet fault/recovery during live TCP activity and process restart against retained tmpfs data. It does not model machine reboot, independent kernel failures or physical disk durability.

The planted missing acknowledgement produces a checker-specific failure for `k/99/000000@0`, with one missing acknowledged write, and the expected guest exit 1. It does not exit via the fixed-work incomplete-load guard.

## The 512 MiB failure and resource correction

The initial 512-MiB one-guest workload acknowledged all 128 writes but could not finish checking its members. A longer diagnostic run returned a real checker failure: n1 was unavailable while n2/n3 were at revision 129 with no missing acknowledged values. This was not accepted as a throughput sample.

At five virtual seconds, resource diagnostics showed `/data` tmpfs **247.0 MiB total, 244.3 MiB used (99%), 2.7 MiB free**. n1's log reported **failed to create WAL: no space left on device**. `ps` showed only n2/n3 etcd processes. `MemAvailable` was 84,584 KiB and no OOM was reported. The shared tmpfs quota, rather than an observed revision divergence, explains the dead member. A separate diagnostic world intentionally ends at its horizon; its status 0 is not treated as qualification success.

Increasing **both** layouts' common per-guest memory to 1024 MiB increased their default tmpfs capacities and allowed the complete final fixture to pass. Thus the final comparison is one 1-GiB guest versus four 1-GiB guests, not equal total memory. The complete failed runs and diagnostic logs remain under `raw/results/`; historical prelaunch manifests were not retrospectively invented. Historical rows without contemporaneous manifests are explicitly marked `historical-unrecorded` and do not enter final medians or final-input replay qualification. Their separate historical replay groups remain visible in the summary.

## Build, safety and evidence

- Host: AMD Ryzen 5950X, Linux 6.18.54, assigned CPU 14; SMT sibling 30 unused. Existing campaign VMM workers were checked with `taskset -pc` and were on CPUs 5–12. Their unpinned wrappers could run elsewhere; this was an authorized shared-host relative experiment.
- Source and guest ABI: merged-main VMM with main's ABI1 kernel patch. The compatible read-only cached kernel's `seedbox.c` and `seedbox.h` matched main exactly. No PV kernel was paired with this VMM. Private cached source/objects were copied, Kbuild path references relocated, and the new guest kernel was built with `-j1` on CPU 14. A clean pinned-tarball preparation fallback is provided.
- Guest feature additions: NET_NS, VETH, NET_SCHED, NET_SCH_NETEM, NET_SCH_INGRESS, NET_CLS_MATCHALL, NET_CLS_ACT and NET_ACT_GACT. No host kernel, module, mitigation or service was changed. The kernel hash is `2de1ec4617627d484b151a1ba39a042fd0b36380d252198acae168b4d5fa00a4`; config hash is `f7f085eb0d6e30e0465df8a81497245e5009cb658135442df48e72c3ddf554d0`.
- Dependency bootstrap limitation: the initial static-iproute2 Nix request briefly compiled dependencies through the daemon outside CPU 14 despite bounded jobs/cores. Only that request was stopped. The final dynamic iproute2 7.1 output was already cached; its man output was fetched via substitutes. The binary/loader/library closure is embedded at its exact store paths. Subsequent package resolution uses `--max-jobs 0` to prohibit daemon compilation. No global package installation or authentication change occurred.
- Initial image cleanup failed on read-only Nix closure directories. Cleanup now changes write permissions only inside its private temporary staging directory after archive serialization, then removes it. An initial BusyBox PATH collision was also fixed before final qualification; the failed guest attempt is retained.
- `run_guard.py` is a single root supervisor. It owns one VMM process group, handles TERM/INT/HUP, waits for its children before restoration, records failed attempts and enforces a persistent aggregate budget. It uses the shared per-core lock read-only (Linux protected temporary-file handling forbids root append-opening an existing user file). It never edits another owner's job or MSR snapshot.
- Only CPUs 14 and 30 have their required counter-workaround bit temporarily set. Before/set/end/restore values are logged. Every receipt restored both original values exactly to `0x4480000000000`; no snapshot remained. Restore attempts both CPUs even if one fails and retains the snapshot on any error/readback mismatch. Three mocked cleanup tests cover group-exit races, timeout escalation and partial restore failure; three parser controls reject process failure, duration termination and wrong guest exit despite pass text.
- **All sixteen benchmark/diagnostic attempts, including failures, used 142.771 seconds aggregate command wall time**, under the 360-second bound; the longest was 27.310 seconds. Builds and harmless parser/mock tests are not benchmark runtime. No long hunt was run.
- `raw/results/summary.json` contains full timing, exits, branches, identities, witnesses, digests, console hashes and rejected historical attempts. Per-run `.config.json` and `.inputs.txt` capture final launch inputs. `raw/Cargo.lock`, initial/final kernel configs, source and input hashes, and closure paths retain build provenance. Raw evidence is about 0.4 MiB; large images/binaries remain in the private remote directory.

## Reproduction

Run on the assigned workstation with its core available and no overlapping MSR restoration. The private remote root is `/home/gvickers/seedbox-single-vm-network`; shared `/home/gvickers/seedbox-sim` inputs are read only. A fresh directory should contain the checkout's `sim/` sources plus this directory. Preserve the resolved lock before building:

```sh
cp research/single-vm-network/raw/Cargo.lock sim/vmm/Cargo.lock
CPU=14 taskset -c 14 nix develop ./sim --command bash research/single-vm-network/prepare-kernel.sh
CPU=14 taskset -c 14 nix develop ./sim --command bash research/single-vm-network/build-kernel.sh
CPU=14 taskset -c 14 nix develop ./sim --command bash research/single-vm-network/build-images.sh
CPU=14 taskset -c 14 nix develop ./sim --command bash research/single-vm-network/run.sh controls controls multi single multi single negative fault fault
nix develop ./sim --command python3 research/single-vm-network/analyze.py results > results/summary.json
```

`build-images.sh` regenerates explicit absolute-path worlds using the final 1024-MiB setting. Both kernel preparation and image building accept `SEEDBOX_SHARED`; no `path:.`, shared writes or global tool installs are used. The default persistent `results/budget.json` counts every VMM attempt; the existing remote budget already contains this experiment's 142.771 seconds, so do not reset it to evade the six-minute limit. Cold kernel builds need the pinned Linux 6.12.112 tarball in the shared source directory and verify its published SHA256 before applying main's patch. The restored original kernel configuration is retained separately from the feature-enabled final configuration.

For source-only verification without KVM or hardware MSR access:

```sh
python3 research/single-vm-network/test_cleanup.py
python3 research/single-vm-network/test_acceptance.py
bash -n research/single-vm-network/*.sh research/single-vm-network/*.init
```
