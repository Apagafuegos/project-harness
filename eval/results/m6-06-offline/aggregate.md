# Compaction evaluation aggregate

- harness commit: `cd14762332ad459617bd518ed9f55d08a0cab0ec`
- runs: 12
- tasks: 4
- strategies: 3
- estimators: heuristic bytes4-flat-media-v1

## Per strategy

| strategy | version | runs | requirement retention (projection) | requirement retention (probe) | file recall (projection) | file recall (probe) | completion | tokens before | tokens after | failures |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| rolling_summary | 1 | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2317 | 1661 | 0 |
| observation_masking | 1 | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2321 | 2459 | 0 |
| structured_state | 1 | 4 | 12/12 (1.000) | 12/12 (1.000) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2318 | 2338 | 0 |

## Per task

| task | shape | strategy | retention (projection) | retention (probe) | recall (projection) | recall (probe) | completion |
| --- | --- | --- | --- | --- | --- | --- | --- |
| bugfix-with-test | bug_fix | rolling_summary | 3/3 (1.000) | 3/3 (1.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| bugfix-with-test | bug_fix | observation_masking | 3/3 (1.000) | 3/3 (1.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| bugfix-with-test | bug_fix | structured_state | 3/3 (1.000) | 3/3 (1.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| feature-style-rule | feature | rolling_summary | 3/3 (1.000) | 3/3 (1.000) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| feature-style-rule | feature | observation_masking | 3/3 (1.000) | 3/3 (1.000) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| feature-style-rule | feature | structured_state | 3/3 (1.000) | 3/3 (1.000) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| multi-file-change | multi_file | rolling_summary | 3/3 (1.000) | 3/3 (1.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| multi-file-change | multi_file | observation_masking | 3/3 (1.000) | 3/3 (1.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| multi-file-change | multi_file | structured_state | 3/3 (1.000) | 3/3 (1.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| refactor-constraint | refactor | rolling_summary | 3/3 (1.000) | 3/3 (1.000) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| refactor-constraint | refactor | observation_masking | 3/3 (1.000) | 3/3 (1.000) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| refactor-constraint | refactor | structured_state | 3/3 (1.000) | 3/3 (1.000) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
