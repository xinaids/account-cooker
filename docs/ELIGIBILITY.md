# Eligibility

Self-audit of why this submission qualifies for the Superteam Brasil
"Privacy-Through-Noise" bounty, judged by @kauenet. Written so the judge
doesn't have to verify this independently — see `marcelofeitoza`'s account-cooker
#2 for the reference this document format is modeled on.

## Bounty and submission route

- **Bounty:** Privacy-Through-Noise, Superteam Brasil.
- **Repo:** [`solanabr/account-cooker`](https://github.com/solanabr/account-cooker)
  — a sponsor-designated target repo for this bounty, not a standalone
  submission.
- **Submission:** [PR #1](https://github.com/solanabr/account-cooker/pull/1),
  opened from a personal fork ([`xinaids/account-cooker`](https://github.com/xinaids/account-cooker),
  branch `feat/jupiter-swap-scaffold`) against `solanabr/account-cooker`'s
  default branch — the standard GitHub PR submission modality for this
  bounty program.

## Language

- Entire implementation is Rust (`Cargo.toml`, `src/**/*.rs`), matching the
  bounty's stated language requirement. No non-Rust runtime logic (shell
  scripts here are test/CI harnesses around the Rust binaries, not part of
  the deliverable itself).

## Region (Brasil requirement)

- Superteam Brasil bounties require the submitter to be eligible under
  Superteam Brasil's program terms (typically residency/participation tied
  to Brazil). This is an attestation by the account holder (`xinaids`,
  submitting under the email on file with Superteam), not something this
  document can verify from the repo alone — the judge should confirm this
  against the Superteam Earn submission profile, which carries the
  authoritative KYC/region data for this bounty, rather than trusting this
  file for that specific fact.

## License and originality

- MIT license, present at repo root (`LICENSE`), required for the sponsor
  to be able to use/fork the submission.
- Clean-room implementation: no vendored code from another team's
  submission, no third-party dataset, no calls to a paid/private API
  beyond the public Jupiter Swap API and public Solana RPC — see
  "Provenance" in `THREAT_MODEL.md` for how the code was actually produced.

## Scope fit to the bounty brief

- Addresses the stated theme (behavioral/timing noise to defeat wallet
  clustering) with two real protocol integrations (`jupiter_swap`,
  `marinade_stake`), not a single-protocol toy.
- Does **not** claim to solve the sibling problem (value-channel
  unlinkability) — that is explicitly out of scope per `THREAT_MODEL.md`
  and left to `supersonic-tx` in the same bounty, so as not to overclaim
  scope this submission didn't actually cover.

## What this submission does NOT claim (see THREAT_MODEL.md and README for detail)

- Not validated against a real labeled human-wallet dataset (none available).
- Surfpool-based multi-agent soak (mainnet-mirror, thousands of agents,
  multi-run reproducibility) was **not completed** — attempted, blocked by
  a missing system dependency (`libclang`, required to build `rocksdb-sys`
  for `surfpool-cli`) in the available environment, and not worked around
  by inflating scale claims instead. See "Known limitations" in `README.md`.
