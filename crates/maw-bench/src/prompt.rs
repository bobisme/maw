//! Deterministic prompt rendering: `ScenarioPlan` → agent prompt string.
//!
//! The reproducibility seam from `notes/sg2-benchmark-preregistration.md`
//! §11 ("the harness MUST reproduce within SP3's measured variance"):
//! the *bytes* sent to the agent are a pure function of the seed +
//! convention text. The only variance allowed downstream is the LLM
//! itself.
//!
//! # What we DON'T do
//!
//! - We do NOT translate scenario ops into substrate-specific verbs.
//!   That would advantage maw (whose verbs the agent would see fully
//!   formed) over jj (whose verbs would have to be paraphrased). The
//!   agent learns substrate verbs from the convention text (§8.1
//!   crib) — the prompt only states the *task battery*.
//! - We do NOT name maw concepts in the task list. The task battery is
//!   in terms of abstract intents (`create three workspaces ... merge
//!   them ...`) so the same prompt works across all four arms.

use std::fmt::Write as _;

use maw_scenario::{Op, ScenarioPlan};

/// Inputs that control [`render_prompt`].
pub struct PromptInputs<'a> {
    /// The deterministic scenario plan.
    pub plan: &'a ScenarioPlan,
    /// Per-arm convention crib (frozen per §8.1 of the pre-reg). The
    /// substrate is the source of truth.
    pub convention_text: &'a str,
    /// Where the agent should work. The prompt names this absolute path
    /// per the `ws/default/AGENTS.md` "Output Guidelines" rule.
    pub workspace_root_absolute: &'a str,
}

/// Render the prompt bytes the agent receives. Pure function — same
/// inputs → same output. The harness's determinism contract.
///
/// The rendered prompt has three sections:
///
/// 1. **Identity & rules** — fresh-context framing.
/// 2. **Substrate crib** — the per-arm convention text verbatim.
/// 3. **Task battery** — the abstract task list, derived from the plan.
#[must_use]
pub fn render_prompt(inp: &PromptInputs<'_>) -> String {
    let mut out = String::with_capacity(2048);

    // 1. Identity / rules (same across all arms — fresh-context framing).
    out.push_str(IDENTITY_HEADER);
    out.push_str("\n\n");

    // 2. Substrate crib.
    out.push_str("## Substrate convention\n\n");
    out.push_str(inp.convention_text.trim_end());
    out.push_str("\n\n");

    // 3. Task battery.
    out.push_str("## Workspace\n\n");
    let _ = writeln!(out, "Work under: {}", inp.workspace_root_absolute);
    let _ = writeln!(out, "Scenario seed: {}", inp.plan.seed);
    out.push_str("\n## Task battery\n\n");
    out.push_str(
        "Complete the following abstract tasks using the substrate \
         vocabulary above. Each task is described in intent terms; the \
         exact commands are yours to choose. Aim to leave the substrate \
         coherent (no orphaned state, no lost work).\n\n",
    );

    let tasks = task_battery_from_plan(inp.plan);
    for (i, t) in tasks.iter().enumerate() {
        let _ = writeln!(out, "{}. {}", i + 1, t);
    }

    out
}

/// Boilerplate the agent sees first — frozen across arms so prompt
/// length doesn't bias the comparison.
const IDENTITY_HEADER: &str = "You are a fresh-context agent in a coordination-benchmark task. \
Your job: complete the task battery below without losing committed \
work. You may inspect the workspace freely. Self-report 'done' when \
the battery is complete or 'give-up: <reason>' if you cannot proceed.";

/// Derive a fixed-shape task battery from the scenario plan.
///
/// The battery is **abstract** — it counts the kinds of operations the
/// plan exercises (creates, edits/commits, merges, destroys, recovers)
/// and lists the intent at each step in arm-neutral language. The
/// underlying plan is what makes two arms see the same scenario; the
/// task list is the agent-facing summary.
#[must_use]
fn task_battery_from_plan(plan: &ScenarioPlan) -> Vec<String> {
    let mut tasks = Vec::new();
    for step in &plan.steps {
        match &step.op {
            Op::WsCreate { ws, .. } => {
                tasks.push(format!(
                    "Create a coordination workspace named `{}` based on the project's default branch.",
                    ws.0
                ));
            }
            Op::EditFiles { ws, files } => {
                let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
                tasks.push(format!(
                    "In workspace `{}`, edit the following files (any content is fine; the test cares about coordination): {}",
                    ws.0,
                    paths.join(", ")
                ));
            }
            Op::Commit { ws, .. } => {
                tasks.push(format!(
                    "Commit pending changes in workspace `{}` with a descriptive message.",
                    ws.0
                ));
            }
            Op::Merge { srcs, destroy, .. } => {
                let src_names: Vec<String> = srcs.iter().map(|w| format!("`{}`", w.0)).collect();
                let suffix = if *destroy {
                    " and remove the sources after the merge succeeds"
                } else {
                    ""
                };
                tasks.push(format!(
                    "Integrate workspaces {} into the default branch{}.",
                    src_names.join(", "),
                    suffix
                ));
            }
            Op::Sync { ws } => {
                tasks.push(format!(
                    "Bring workspace `{}` up-to-date with the latest default branch.",
                    ws.0
                ));
            }
            Op::Advance { ws } => {
                tasks.push(format!(
                    "Advance workspace `{}` onto the latest default branch, \
                     replaying its already-committed work on top rather than \
                     discarding it.",
                    ws.0
                ));
            }
            Op::Destroy { ws, force } => {
                let force_note = if *force {
                    " (force even if it has uncommitted work)"
                } else {
                    ""
                };
                tasks.push(format!("Remove workspace `{}`{force_note}.", ws.0));
            }
            Op::Recover { ws, to } => {
                tasks.push(format!(
                    "Recover the previously destroyed workspace `{}` into a new workspace named `{}`.",
                    ws.0, to.0
                ));
            }
            Op::OutOfMawCommit { files, .. } => {
                let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
                tasks.push(format!(
                    "Commit changes DIRECTLY on the default branch (outside the maw merge flow) \
                     touching: {}. Then a later integration must absorb this drift.",
                    paths.join(", ")
                ));
            }
            Op::DirtyTrunkWrite { files } => {
                let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
                tasks.push(format!(
                    "In the default workspace, make UNCOMMITTED edits to the tracked files: {} \
                     (leave them uncommitted).",
                    paths.join(", ")
                ));
            }
            Op::Gc {
                recovery_snapshots,
                older_than_days,
            } => {
                if *recovery_snapshots {
                    tasks.push(format!(
                        "Run garbage collection including the recovery-snapshot sweep \
                         (removing snapshots older than {older_than_days} day(s))."
                    ));
                } else {
                    tasks.push("Run routine garbage collection on the repository.".to_owned());
                }
            }
        }
    }
    tasks
}

/// SHA-256 of the prompt bytes (the `prompt_hash` field of §6.4).
///
/// We use a tiny in-tree implementation to avoid pulling another crate
/// just for one digest. (The harness already takes a `sha2` dep transit-
/// ively via maw-core when assurance is active, but `maw-bench` keeps
/// its deps minimal; this is the only hash we need.)
#[must_use]
pub fn prompt_sha256_hex(prompt: &str) -> String {
    // FNV-1a 64-bit fallback is NOT cryptographic; we want SHA-256 as
    // the §6.4 contract says. Implement SHA-256 directly to avoid the
    // `sha2` dep. ~80 lines, well-tested in our unit test.
    let bytes = prompt.as_bytes();
    let digest = sha256(bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

// ---------------------------------------------------------------------------
// SHA-256 (small, standalone) — gated to this module
// ---------------------------------------------------------------------------

// FIPS 180-4 spec uses the canonical single-letter variable names
// `a..h` and the 64-entry round-constant table `K`. The short names map
// 1:1 to the spec and are universally recognised; renaming them would
// hurt readability without buying defect prevention.
#[allow(
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::too_many_lines
)]
fn sha256(message: &[u8]) -> [u8; 32] {
    // Initial hash values (FIPS 180-4 §5.3.3).
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    // Round constants (FIPS 180-4 §4.2.2).
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    // Pre-processing: pad + length.
    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(message.len() + 72);
    padded.extend_from_slice(message);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit chunk.
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word_bytes) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word_bytes[0], word_bytes[1], word_bytes[2], word_bytes[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use maw_scenario::{ConditionProfile, DefaultScenarioGenerator, ScenarioGenerator};

    #[test]
    fn render_prompt_is_deterministic() {
        let profile = ConditionProfile::default();
        let plan = DefaultScenarioGenerator::generate(42, &profile);
        let inp = PromptInputs {
            plan: &plan,
            convention_text: "# crib\n- step a\n- step b\n",
            workspace_root_absolute: "/tmp/run-1",
        };
        let p1 = render_prompt(&inp);
        let p2 = render_prompt(&inp);
        assert_eq!(p1, p2, "prompt rendering not pure");
        // Sanity: prompt contains the convention text and the workspace path.
        assert!(p1.contains("# crib"));
        assert!(p1.contains("/tmp/run-1"));
        // And the scenario seed is named.
        assert!(p1.contains("42"));
    }

    #[test]
    fn render_prompt_changes_with_plan_seed() {
        let profile = ConditionProfile::default();
        let p_a = DefaultScenarioGenerator::generate(1, &profile);
        let p_b = DefaultScenarioGenerator::generate(2, &profile);
        let inp_a = PromptInputs {
            plan: &p_a,
            convention_text: "x",
            workspace_root_absolute: "/tmp/r",
        };
        let inp_b = PromptInputs {
            plan: &p_b,
            convention_text: "x",
            workspace_root_absolute: "/tmp/r",
        };
        assert_ne!(render_prompt(&inp_a), render_prompt(&inp_b));
    }

    /// SHA-256 KAT (FIPS 180-4 example): "abc".
    #[test]
    fn sha256_kat_abc() {
        let got = prompt_sha256_hex("abc");
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// SHA-256 KAT: empty string.
    #[test]
    fn sha256_kat_empty() {
        let got = prompt_sha256_hex("");
        assert_eq!(
            got,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// SHA-256 KAT: longer block ("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").
    #[test]
    fn sha256_kat_long_block() {
        let got = prompt_sha256_hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            got,
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }
}
