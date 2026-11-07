# Executed share: functions rustd compiled against functions that ran

Measured 2026-11-07T15:32:42+03:00 by the callgrind method (`harness/executed.py`), one cold `cargo rustd build` per subject, then the session commands under valgrind. A function counts as executed when callgrind charged it any instruction. Test suites are not included.

| Subject | compiled roots (+instances) | ran, first command | ran, whole session | share, first | share, session |
|---|---:|---:|---:|---:|---:|
| hello | 2 (+7) | 2 (+6) | 2 (+6) | 88.9 % | 88.9 % |
| ripgrep | 711 (+14,978) | 361 (+4,030) | 431 (+4,949) | 28.0 % | 34.3 % |
| fd | 102 (+9,030) | 47 (+2,749) | 54 (+2,881) | 30.6 % | 32.1 % |
| hyperfine | 66 (+5,329) | 48 (+1,580) | 50 (+1,786) | 30.2 % | 34.0 % |
| bat | 203 (+10,607) | 69 (+2,399) | 93 (+2,919) | 22.8 % | 27.9 % |
| nushell | 2,976 (+146,260) | 566 (+8,127) | 623 (+10,730) | 5.8 % | 7.6 % |
| genws | 20,403 (+308,655) | 20,403 (+214,050) | 20,403 (+214,050) | 71.2 % | 71.2 % |
