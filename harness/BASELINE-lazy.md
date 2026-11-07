# Baseline: the first build through `cargo rustd` against plain cargo

Measured 2026-11-07T22:31:24+03:00 on Intel(R) Core(TM) Ultra 7 155H, cores 0-3 of 22, 15.0 GB, kernel 7.2.7-arch1-1, Rust 1.99.0, rustd `/home/sura/repo/oss/rustd/target/release/rustd` (daemon version 0.0.0-462bde0-18e5d9bf4e76efdb), harness 42d25345d4, Cranelift, sccache off except in its own event, `jobs = 4` and the linker from the user's cargo config. Seconds, the median over the runs after a warm-up, with the minimum and maximum in brackets. Phases are the worker's own timings.

## deps

| Subject | runs | cargo build | rustd build | spare | load | resolve | entries | compile | emit | link | functions (+instances) | daemon RSS MB | to main cargo / rustd ms | run 1 / run 2 rustd s | run 1 cargo s | output |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| hello | 5 | 0.14 (0.14 to 0.15) | 6.10 (5.79 to 6.86) | 3.20 (2.90 to 3.60) | 0.36 (0.28 to 0.37) | 1.50 (1.40 to 1.70) | 0.00 (0.00 to 0.00) | 0.94 (0.90 to 0.96) | 0.00 (0.00 to 0.00) | 0.05 (0.04 to 0.06) | 1 (+7) | 1182 | 0.9 / 0.9 | 0.00 (0.00 to 0.00) / 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | same |
| ripgrep | 5 |  | 7.67 (7.08 to 7.89) | 3.50 (3.20 to 3.70) | 0.42 (0.39 to 0.47) | 1.50 (1.50 to 1.70) | 0.00 (0.00 to 0.02) | 2.00 (1.70 to 2.10) | 0.00 (0.00 to 0.00) | 0.08 (0.07 to 0.08) | 1 (+78) | 2000 |  / 84.8 | 0.08 (0.07 to 0.09) / 0.09 (0.09 to 0.09) |  | same |
| fd | 5 |  | 7.55 (7.03 to 8.18) | 3.20 (3.00 to 3.60) | 0.50 (0.44 to 0.52) | 2.00 (2.00 to 2.20) | 0.00 (0.00 to 0.00) | 1.50 (1.20 to 1.80) | 0.00 (0.00 to 0.00) | 0.06 (0.05 to 0.07) | 1 (+8) | 1809 |  / 88.2 | 0.08 (0.08 to 0.09) / 0.09 (0.08 to 0.09) |  | same |
| hyperfine | 5 |  | 7.79 (7.68 to 8.40) | 3.10 (3.00 to 3.30) | 0.50 (0.48 to 0.55) | 2.20 (2.20 to 2.40) | 0.00 (0.00 to 0.01) | 1.70 (1.60 to 2.10) | 0.00 (0.00 to 0.00) | 0.08 (0.07 to 0.14) | 1 (+31) | 1723 |  / 81.6 | 0.09 (0.08 to 0.09) / 0.08 (0.07 to 0.09) |  | same |
| bat | 5 |  | 8.23 (8.10 to 9.26) | 3.00 (3.00 to 3.10) | 0.69 (0.62 to 0.71) | 2.60 (2.40 to 2.70) | 0.00 (0.00 to 0.00) | 1.70 (1.70 to 2.30) | 0.00 (0.00 to 0.00) | 0.15 (0.12 to 0.24) | 1 (+120) | 2148 |  / 87.7 | 0.09 (0.07 to 0.10) / 0.09 (0.08 to 0.10) |  | same |
| nushell | 3 |  | 48.01 (47.91 to 48.75) | 8.00 (6.40 to 8.00) | 3.20 (3.10 to 3.20) | 27.50 (27.00 to 28.20) | 0.00 (0.00 to 0.00) | 8.80 (8.70 to 8.90) | 0.01 (0.01 to 0.01) | 0.74 (0.63 to 0.87) | 5 (+4775) | 9052 |  / 146.9 | 0.14 (0.13 to 0.16) / 0.14 (0.14 to 0.15) |  | same |
| genws | 1 |  | 20.35 (20.35 to 20.35) | 7.60 (7.60 to 7.60) | 0.59 (0.59 to 0.59) | 4.30 (4.30 to 4.30) | 0.02 (0.02 to 0.02) | 7.40 (7.40 to 7.40) | 0.00 (0.00 to 0.00) | 0.13 (0.13 to 0.13) | 1 (+107) | 7407 |  / 97.1 | 0.10 (0.10 to 0.10) / 0.09 (0.09 to 0.09) |  | same |

## Ceiling: the lazy first build with the dependencies built

Cargo and the eager rustd build are stage 1's (`baseline.json`); the lazy build compiles the eager set and stubs every other function the program reaches, so its time is the least a build that compiles on first call can take before the program runs. The eager build beside it is of the same rustd, built in turn with it; stage 1's is of the rustd it measured. `lazy exit` is the representative run's: -4 is a stub reached.

| Subject | cargo (stage 1) | rustd eager (stage 1) | rustd eager | rustd lazy | saved | roots | compile | of which stubs | object | link | eager fns (+instances) | lazy fns (+instances) | stubbed | lazy exit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| hello | 0.20 (0.18 to 0.22) | 10.42 (10.39 to 10.57) | 6.00 (5.82 to 6.49) | 6.10 (5.79 to 6.86) | -0.10 | 0.00 (0.00 to 0.00) | 0.94 (0.90 to 0.96) | 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | 0.05 (0.04 to 0.06) | 1 (+7) | 1 (+7) | 0 | 0 |
| ripgrep | 5.76 (5.74 to 5.86) | 19.38 (19.33 to 19.49) | 12.04 (11.79 to 12.58) | 7.67 (7.08 to 7.89) | 4.38 | 0.19 (0.15 to 0.20) | 2.00 (1.70 to 2.10) | 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | 0.08 (0.07 to 0.08) | 1769 (+14977) | 1 (+78) | 2 | -4 |
| fd | 2.75 (2.65 to 2.76) | 17.66 (17.59 to 18.10) | 10.51 (10.19 to 10.88) | 7.55 (7.03 to 8.18) | 2.96 | 0.03 (0.03 to 0.04) | 1.50 (1.20 to 1.80) | 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | 0.06 (0.05 to 0.07) | 137 (+9029) | 1 (+8) | 2 | -4 |
| hyperfine | 1.85 (1.84 to 1.88) | 16.10 (15.90 to 16.15) | 9.55 (8.89 to 10.22) | 7.79 (7.68 to 8.40) | 1.76 | 0.03 (0.02 to 0.04) | 1.70 (1.60 to 2.10) | 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | 0.08 (0.07 to 0.14) | 118 (+5329) | 1 (+31) | 1 | -4 |
| bat | 4.57 (4.48 to 4.58) | 20.84 (20.53 to 20.95) | 12.22 (11.89 to 12.64) | 8.23 (8.10 to 9.26) | 3.99 | 0.04 (0.04 to 0.05) | 1.70 (1.70 to 2.30) | 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | 0.15 (0.12 to 0.24) | 292 (+10607) | 1 (+120) | 2 | -4 |
| nushell | 60.77 (60.66 to 61.51) | 142.64 (141.31 to 143.40) | 155.79 (154.44 to 156.17) | 48.01 (47.91 to 48.75) | 107.78 | 1.20 (1.10 to 1.20) | 8.80 (8.70 to 8.90) | 0.02 (0.02 to 0.03) | 0.14 (0.13 to 0.15) | 0.74 (0.63 to 0.87) | 7697 (+146259) | 5 (+4775) | 154 | -4 |
| genws | 57.12 (57.12 to 57.12) | 145.80 (145.80 to 145.80) | 212.32 (212.32 to 212.32) | 20.35 (20.35 to 20.35) | 191.98 | 4.10 (4.10 to 4.10) | 7.40 (7.40 to 7.40) | 0.00 (0.00 to 0.00) | 0.00 (0.00 to 0.00) | 0.13 (0.13 to 0.13) | 25154 (+308655) | 1 (+107) | 30 | -4 |

## Differences and failures

none
