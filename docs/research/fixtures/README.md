# Captured fixtures

Real responses, captured from a conductor built from this working tree on 2026-11-12, so that
future work has the *actual* wire shape instead of a remembered one. Every panel/router contract
defect found in this workstream was invisible to the unit tests and only appeared when a real
payload met a real server; these files are the record of what that server said.

| file | request |
| --- | --- |
| `gpu-catalog-live.json` | `GET /api/gpu/catalog` — 11 cards, prices, plans, quant support |
| `gpu-probe-live.json` | `POST /api/gpu/probe` for `orcarouter/Qwen3.8-27B-Uncensored-FP8` on `RTX-PRO-6000` |
| `gpu-apply-response-live.json` | `POST /api/gpu/apply` `action: "create-deployment"`, HTTP 200 |
| `gpu-rentals-live.json` | `GET /api/gpu/rentals` after that apply |
| `lanes-live.json` | `GET /api/lanes`, trimmed to the two lanes that remain (Modal, Kaggle) |

The captures of the Cerebrium bundle and compute routes were deleted with those routes: they
documented a lane this router no longer offers, and nothing read them.

How they were captured: a second conductor instance on `127.0.0.1:20999` with its own
`MANAROUTER_CONFIG_DIR` / `MANAROUTER_STATE_DIR` under `/tmp`, addressed with `python3 -c` +
`urllib` so that no redirect, cookie or retry hid the real status line. The recipe is in
`ARCHITECTURE-gpu-rentals.md` §10.1.

**The credential in these captures is synthetic.** `wk-SYNTHETICID0001` /
`SYNTHETICsecret0001` were invented for the test and only ever existed in that throwaway vault.
The vault stores the combined string, so what a response can echo is a masked fingerprint
(`wk…0001`) and nothing more — `gpu-apply-response-live.json` shows exactly that and no secret.

These captures predate the one-account-per-rental / one-card fixes: `batch_gpu` and any
`"mode": "interactive"` in them are historical wire shapes, not the current contract. They are kept
verbatim because a capture edited to match today's code would no longer be evidence.

Notes on what these files prove, beyond being fixtures:

- `gpu-rentals-live.json` → `deployments[0].fit` reports `kv_pool_tokens: 1784064` and
  `sessions_at_max_len: 6`. The *aligned* pool, i.e. whole blocks of 64, which is the figure
  vLLM prints at startup — not the raw division remainder.
- `gpu-apply-response-live.json` → `command` is a **top-level** key, a sibling of `bundle`, and
  `bundle.text` is Python that parses (`ast.parse`). An earlier version emitted `""fp8""`.
- `gpu-probe-live.json` → `sizing.vram_source: "datasheet"`: 96 GiB is the nominal figure, and the
  usable 95 GiB is carried separately in `usable_vram_bytes`.
