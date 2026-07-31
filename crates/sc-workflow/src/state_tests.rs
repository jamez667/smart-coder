use super::*;

fn temp(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("dc-wf-{tag}-{n}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn next_phase_walks_the_pipeline() {
    let mut s = WorkflowState::new("do a thing");
    assert_eq!(s.next_phase(), Some(Phase::Specs));
    s.set(Artifact::draft(Phase::Specs, "spec body"));
    assert_eq!(s.next_phase(), Some(Phase::Architecture));
}

#[test]
fn approved_returns_only_approved_in_order() {
    let mut s = WorkflowState::new("t");
    s.set(Artifact::draft(Phase::Specs, "s"));
    s.set(Artifact::draft(Phase::Architecture, "a"));
    s.approve(Phase::Specs);
    let approved = s.approved();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].phase, Phase::Specs);
}

#[test]
fn invalidate_from_drops_at_and_after() {
    let mut s = WorkflowState::new("t");
    for p in Phase::ALL {
        s.set(Artifact::draft(p, "x"));
    }
    s.invalidate_from(Phase::Layout);
    assert!(s.artifact(Phase::Architecture).is_some());
    assert!(s.artifact(Phase::Layout).is_none());
    assert!(s.artifact(Phase::WorkDecomposition).is_none());
}

#[test]
fn is_complete_requires_all_approved() {
    let mut s = WorkflowState::new("t");
    for p in Phase::ALL {
        s.set(Artifact::draft(p, "x"));
        s.approve(p);
    }
    assert!(s.is_complete());
    // A single un-approved phase breaks completion.
    s.set(Artifact::draft(Phase::Layout, "redo"));
    assert!(!s.is_complete());
}

#[test]
fn save_to_openspec_dir_uses_named_files() {
    let ws = temp("openspec");
    let dir = ws.join("specs").join("my-feature");
    let mut s = WorkflowState::new("build it");
    s.set(Artifact::draft(Phase::Specs, "# the spec"));
    s.set(Artifact::draft(Phase::Architecture, "# the arch"));
    save_to(&dir, &mut s, true).unwrap();
    assert!(dir.join("spec.md").is_file(), "spec.md written");
    assert!(
        dir.join("architecture.md").is_file(),
        "architecture.md written"
    );
    assert!(
        !dir.join("01-specs.md").exists(),
        "no numbered names in openspec mode"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("architecture.md")).unwrap(),
        "# the arch"
    );
}

#[test]
fn save_then_load_round_trips_and_writes_markdown() {
    let ws = temp("persist");
    let mut s = WorkflowState::new("build a parser");
    s.set(Artifact::draft(Phase::Specs, "# Specs\nbuild it"));
    s.approve(Phase::Specs);
    save(&ws, &mut s).unwrap();

    // The per-phase Markdown is on disk and reviewable.
    let md = std::fs::read_to_string(plan_dir(&ws).join("01-specs.md")).unwrap();
    assert!(md.contains("build it"));

    let loaded = load(&ws).unwrap().unwrap();
    assert_eq!(loaded, s);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn load_from_reads_state_from_a_custom_dir_for_build_resume() {
    // The Build-resume path: a Breakdown saved its approved design into specs/<slug>/; a later
    // Build must load THAT state.json (not the default plan dir) so the runner skips redesign.
    let ws = temp("resume");
    let dir = ws.join("specs").join("alt-seats");
    let mut s = WorkflowState::new("build the seat picker");
    for p in [
        Phase::Specs,
        Phase::Architecture,
        Phase::Layout,
        Phase::StageBreakdown,
    ] {
        s.set(Artifact::draft(p, format!("# {}", p.title())));
        s.approve(p);
    }
    save_to(&dir, &mut s, true).unwrap();

    // load_from finds it; the approved design phases come back, so next_phase() skips straight
    // to WorkDecomposition (the only un-generated phase) instead of re-running Architecture.
    let loaded = load_from(&dir).unwrap().unwrap();
    assert_eq!(loaded.approved().len(), 4, "all four design phases reused");
    assert_eq!(loaded.next_phase(), Some(Phase::WorkDecomposition));
    // A missing dir is a clean None (fresh design), not an error.
    assert!(load_from(&ws.join("nope")).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn feedback_is_set_persisted_and_cleared_on_approve() {
    let ws = temp("feedback");
    let mut s = WorkflowState::new("t");
    s.set(Artifact::draft(Phase::Architecture, "draft"));
    s.set_feedback(Phase::Architecture, "make it event-driven");
    assert_eq!(
        s.feedback(Phase::Architecture),
        Some("make it event-driven")
    );

    // Survives a save/load round-trip.
    save(&ws, &mut s).unwrap();
    let loaded = load(&ws).unwrap().unwrap();
    assert_eq!(
        loaded.feedback(Phase::Architecture),
        Some("make it event-driven")
    );

    // Approving the phase clears its feedback — the note has done its job.
    s.approve(Phase::Architecture);
    assert_eq!(s.feedback(Phase::Architecture), None);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn load_missing_is_none() {
    let ws = temp("missing");
    assert!(load(&ws).unwrap().is_none());
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_stale_writer_cannot_clobber_a_decision_someone_else_made() {
    // THE failure spec 19 describes, reproduced directly:
    //
    //   "The daemon parks a run at the specs gate. The developer gets home,
    //    opens the same project in the GUI, and starts a plan run on the same
    //    task. Both hold divergent copies. The phone-side approval is written,
    //    then silently clobbered by the GUI's next phase save. Neither process
    //    notices; nothing logs it."
    //
    // Before compare-and-swap, A's second save won and B's approval vanished
    // with no error anywhere. Now A is refused, loudly.
    let ws = temp("clobber");
    let dir = ws.join("specs").join("shared");

    // Process A: saves a draft, then blocks at its gate holding this copy.
    let mut a = WorkflowState::new("build the thing");
    a.set(Artifact::draft(Phase::Specs, "# draft from A"));
    save_to(&dir, &mut a, true).unwrap();

    // Process B: loads, approves, saves. This is the decision that must survive.
    let mut b = load_from(&dir).unwrap().unwrap();
    b.approve(Phase::Specs);
    b.set(Artifact::draft(Phase::Architecture, "# B's architecture"));
    save_to(&dir, &mut b, true).unwrap();

    // Process A wakes and saves its now-stale copy.
    let err = save_to(&dir, &mut a, true).expect_err("a stale write must be refused");
    let msg = err.to_string();
    assert!(msg.contains("changed underneath"), "{msg}");
    assert!(msg.contains("generation"), "names both generations: {msg}");

    // B's work is intact on disk — the whole point.
    let on_disk = load_from(&dir).unwrap().unwrap();
    assert!(
        on_disk.artifact(Phase::Specs).unwrap().is_approved(),
        "B's approval survived"
    );
    assert!(
        on_disk.artifact(Phase::Architecture).is_some(),
        "B's architecture survived"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_refused_save_leaves_the_directory_untouched() {
    // The check runs before anything is written, so a rejected save cannot
    // leave half its artifacts on disk for the winner to trip over.
    let ws = temp("refused-clean");
    let dir = ws.join("specs").join("s");

    let mut a = WorkflowState::new("t");
    a.set(Artifact::draft(Phase::Specs, "# A"));
    save_to(&dir, &mut a, true).unwrap();

    let mut b = load_from(&dir).unwrap().unwrap();
    b.approve(Phase::Specs);
    save_to(&dir, &mut b, true).unwrap();

    // A now tries to write a phase B never had.
    a.set(Artifact::draft(Phase::Architecture, "# A's architecture"));
    assert!(save_to(&dir, &mut a, true).is_err());
    assert!(
        !dir.join("architecture.md").exists(),
        "nothing from the refused save reached disk"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn the_generation_advances_with_every_save() {
    let ws = temp("generation");
    let mut s = WorkflowState::new("t");
    assert_eq!(s.generation(), 0, "a fresh state has written nothing");

    s.set(Artifact::draft(Phase::Specs, "x"));
    save(&ws, &mut s).unwrap();
    assert_eq!(s.generation(), 1);
    save(&ws, &mut s).unwrap();
    assert_eq!(s.generation(), 2);

    // And it survives the round-trip, or the next process starts from zero and
    // every save after a reload would look stale.
    assert_eq!(load(&ws).unwrap().unwrap().generation(), 2);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn state_json_written_before_the_counter_existed_loads_as_generation_zero() {
    // No migration: every file on disk today has no `generation` field, and
    // must keep loading rather than failing to parse.
    let ws = temp("legacy");
    let dir = plan_dir(&ws);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("state.json"),
        r#"{"task":"an old run","artifacts":[],"feedback":[]}"#,
    )
    .unwrap();

    let loaded = load(&ws).unwrap().expect("legacy state still loads");
    assert_eq!(loaded.task, "an old run");
    assert_eq!(loaded.generation(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_resumed_run_can_continue_its_predecessors_generation() {
    // A resuming run rebuilds from adopted artifacts, so it starts at 0 and
    // would look stale to the state it just resumed from. Adopting the number
    // is what lets the Build path keep saving (it holds the lease, so this is a
    // continuation rather than a race).
    let ws = temp("adopt");
    let dir = ws.join("specs").join("s");
    let mut first = WorkflowState::new("t");
    first.set(Artifact::draft(Phase::Specs, "# spec"));
    first.approve(Phase::Specs);
    save_to(&dir, &mut first, true).unwrap();
    save_to(&dir, &mut first, true).unwrap();
    assert_eq!(first.generation(), 2);

    // Without adopting, the fresh state is refused.
    let prior = load_from(&dir).unwrap().unwrap();
    let mut resumed = WorkflowState::new("t");
    for a in prior.approved() {
        resumed.set(a.clone());
        resumed.approve(a.phase);
    }
    assert!(
        save_to(&dir, &mut resumed.clone(), true).is_err(),
        "a generation-0 writer looks stale, as it should"
    );

    resumed.adopt_generation(prior.generation());
    save_to(&dir, &mut resumed, true).expect("adopted, so it continues");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_crash_mid_write_leaves_the_previous_state_intact() {
    // `fs::write` truncates first, so a crash (or a full disk) part-way through
    // left a truncated state.json and the run's whole history was gone.
    // Atomic rename means a reader sees either the old file or the new one.
    let ws = temp("atomic");
    let dir = plan_dir(&ws);
    let mut s = WorkflowState::new("the original");
    s.set(Artifact::draft(Phase::Specs, "# the original spec"));
    save(&ws, &mut s).unwrap();

    // Simulate a crashed write: a temp file left behind, target untouched.
    let tmp = dir.join(format!("state.tmp{}", std::process::id()));
    std::fs::write(&tmp, "{ truncated garbage").unwrap();

    let loaded = load(&ws).unwrap().expect("the previous state still loads");
    assert_eq!(loaded.task, "the original");
    assert_eq!(
        loaded.artifact(Phase::Specs).unwrap().content,
        "# the original spec"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn write_atomic_leaves_no_temp_file_behind() {
    // A stray `state.tmp1234` in an artifact directory is confusing at best and
    // gets committed at worst.
    let ws = temp("no-temp");
    let mut s = WorkflowState::new("t");
    s.set(Artifact::draft(Phase::Specs, "x"));
    save(&ws, &mut s).unwrap();

    let leftovers: Vec<String> = std::fs::read_dir(plan_dir(&ws))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_reader_never_sees_a_partially_written_file() {
    // The property atomicity buys, asserted directly: at every moment during a
    // write, the file on disk parses.
    let ws = temp("partial");
    let dir = plan_dir(&ws);
    let mut s = WorkflowState::new("t");
    s.set(Artifact::draft(Phase::Specs, "x".repeat(100_000)));
    save(&ws, &mut s).unwrap();

    // Write a much larger state over it; a truncating write would leave a
    // window where state.json is short. Read back immediately after.
    s.set(Artifact::draft(Phase::Architecture, "y".repeat(200_000)));
    save(&ws, &mut s).unwrap();
    assert!(
        load_from(&dir).unwrap().is_some(),
        "state.json parses after an overwrite"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
