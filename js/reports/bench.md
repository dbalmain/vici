# vici-js speed

Generated: 2026-08-13T21:55:58.485Z

- Node v24.18.1 on linux/x64
- AMD Ryzen 9 9955HX 16-Core Processor, 60 GiB
- Engine rebuild is untimed; `text()` is never called in the timed region.

## Cold start

A fresh process importing the module and constructing one editor: **10 ms** p50 (11 ms mean over 10 processes).

## Hot

| Workload | Engine | Mode | p50 | p95 | mean | iters |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| `insert-1k` | vici-js | bulk | 737 µs | 1.28 ms | 812 µs | 366 |
| `insert-1k` | vici-js | per-key | 716 µs | 1.13 ms | 788 µs | 380 |
| `insert-1k` | index.ts | bulk | 8.41 ms | 8.93 ms | 8.40 ms | 40 |
| `insert-1k` | index.ts | per-key | 8.26 ms | 9.04 ms | 8.23 ms | 40 |
| `words-small` | vici-js | bulk | 14 µs | 34 µs | 18 µs | 400 |
| `words-small` | vici-js | per-key | 8.2 µs | 11 µs | 9.4 µs | 400 |
| `words-small` | index.ts | bulk | 25 µs | 42 µs | 29 µs | 400 |
| `words-small` | index.ts | per-key | 18 µs | 24 µs | 19 µs | 400 |
| `words-100k` | vici-js | bulk | 9.9 µs | 12 µs | 10 µs | 400 |
| `words-100k` | vici-js | per-key | 9.4 µs | 10 µs | 9.8 µs | 389 |
| `words-100k` | index.ts | bulk | 33 µs | 38 µs | 35 µs | 400 |
| `words-100k` | index.ts | per-key | 33 µs | 36 µs | 33 µs | 400 |
| `words-1m` | vici-js | bulk | 14 µs | 22 µs | 15 µs | 40 |
| `words-1m` | vici-js | per-key | 11 µs | 18 µs | 12 µs | 40 |
| `words-1m` | index.ts | bulk | 35 µs | 42 µs | 36 µs | 88 |
| `words-1m` | index.ts | per-key | 34 µs | 39 µs | 35 µs | 89 |
| `delete-word` | vici-js | bulk | 428 µs | 673 µs | 463 µs | 246 |
| `delete-word` | vici-js | per-key | 415 µs | 439 µs | 425 µs | 254 |
| `delete-word` | index.ts | bulk | 4.03 ms | 4.59 ms | 4.09 ms | 68 |
| `delete-word` | index.ts | per-key | 3.86 ms | 4.61 ms | 3.95 ms | 71 |
| `undo-storm` | vici-js | bulk | 305 µs | 363 µs | 316 µs | 400 |
| `undo-storm` | vici-js | per-key | 280 µs | 290 µs | 284 µs | 400 |
| `undo-storm` | index.ts | bulk | 1.66 ms | 2.13 ms | 1.72 ms | 171 |
| `undo-storm` | index.ts | per-key | 1.53 ms | 2.79 ms | 1.67 ms | 177 |
| `macro` | vici-js | bulk | 391 µs | 492 µs | 406 µs | 400 |
| `macro` | vici-js | per-key | 386 µs | 406 µs | 391 µs | 400 |
| `macro` | index.ts | bulk | 815 µs | 1.24 ms | 882 µs | 340 |
| `macro` | index.ts | per-key | 807 µs | 830 µs | 822 µs | 365 |
| `search` | vici-js | bulk | 382 µs | 442 µs | 391 µs | 247 |
| `search` | vici-js | per-key | 376 µs | 384 µs | 377 µs | 260 |
| `search` | index.ts | bulk | 1.15 ms | 1.47 ms | 1.23 ms | 177 |
| `search` | index.ts | per-key | 1.14 ms | 1.17 ms | 1.14 ms | 187 |
| `operator-all` | vici-js | bulk | 28 µs | 54 µs | 38 µs | 373 |
| `operator-all` | vici-js | per-key | 26 µs | 49 µs | 36 µs | 374 |
| `operator-all` | index.ts | bulk | 7.7 µs | 11 µs | 8.6 µs | 400 |
| `operator-all` | index.ts | per-key | 6.4 µs | 8.0 µs | 7.0 µs | 400 |
| `edit-session` | vici-js | bulk | 112 µs | 152 µs | 125 µs | 400 |
| `edit-session` | vici-js | per-key | 100 µs | 111 µs | 101 µs | 400 |
| `edit-session` | index.ts | bulk | 1.85 ms | 4.00 ms | 2.45 ms | 110 |
| `edit-session` | index.ts | per-key | 2.10 ms | 4.39 ms | 2.60 ms | 102 |
