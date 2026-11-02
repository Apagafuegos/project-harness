# Compaction evaluation aggregate

- harness commit: `02bd56187ec57404bed951db383144ec7c2ae459`
- runs: 12
- tasks: 4
- strategies: 3
- estimators: heuristic bytes4-flat-media-v1

## Per strategy

| strategy | version | runs | requirement retention (projection) | requirement retention (probe) | file recall (projection) | file recall (probe) | completion | tokens before | tokens after | failures |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| rolling_summary | 2 | 4 | 8/12 (0.667) | 5/12 (0.417) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2317 | 3003 | 0 |
| observation_masking | 1 | 4 | 12/12 (1.000) | 6/12 (0.500) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2321 | 2459 | 0 |
| structured_state | 2 | 4 | 12/12 (1.000) | 5/12 (0.417) | 6/6 (1.000) | 6/6 (1.000) | 4/4 (1.000) | 2318 | 3825 | 0 |

## Per task

| task | shape | strategy | retention (projection) | retention (probe) | recall (projection) | recall (probe) | completion |
| --- | --- | --- | --- | --- | --- | --- | --- |
| bugfix-with-test | bug_fix | rolling_summary | 3/3 (1.000) | 2/3 (0.667) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| bugfix-with-test | bug_fix | observation_masking | 3/3 (1.000) | 2/3 (0.667) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| bugfix-with-test | bug_fix | structured_state | 3/3 (1.000) | 1/3 (0.333) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| feature-style-rule | feature | rolling_summary | 2/3 (0.667) | 2/3 (0.667) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| feature-style-rule | feature | observation_masking | 3/3 (1.000) | 2/3 (0.667) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| feature-style-rule | feature | structured_state | 3/3 (1.000) | 2/3 (0.667) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| multi-file-change | multi_file | rolling_summary | 0/3 (0.000) | 0/3 (0.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| multi-file-change | multi_file | observation_masking | 3/3 (1.000) | 0/3 (0.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| multi-file-change | multi_file | structured_state | 3/3 (1.000) | 0/3 (0.000) | 2/2 (1.000) | 2/2 (1.000) | 1/1 (1.000) |
| refactor-constraint | refactor | rolling_summary | 3/3 (1.000) | 1/3 (0.333) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| refactor-constraint | refactor | observation_masking | 3/3 (1.000) | 2/3 (0.667) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
| refactor-constraint | refactor | structured_state | 3/3 (1.000) | 2/3 (0.667) | 1/1 (1.000) | 1/1 (1.000) | 1/1 (1.000) |
