//! Synthetic local captures only. The model records the real first builder turn, then stops.
use super::*;
use crate::agent::{ApiConversation, EngineHost, LoopConfig, ModelSlot};
use crate::model::{ChatModel, Delta, Stop, TurnRequest, TurnResult};
use craft_anything_engine::tools::web_common::{Image, encode_png};

struct BoundaryModel(Arc<Mutex<Vec<String>>>, CancelToken, PathBuf);
impl ChatModel for BoundaryModel {
    fn turn(&mut self, req: &TurnRequest<'_>, _: &mut dyn FnMut(Delta), _: &Stop) -> Result<TurnResult, ToolError> {
        let prompt: String = req.messages.iter().map(crate::model::Msg::text).collect();
        let route = self.2.join("routes/account/README.md");
        let private_route = if route.is_file() { std::fs::read_to_string(route).unwrap() } else { String::new() };
        self.0.lock().unwrap().push(format!("{prompt}{private_route}\n{}", std::fs::read_to_string(self.2.join("README.md")).unwrap()));
        self.1.cancel();
        Err(ToolError::new(ErrorCode::Cancelled, "Synthetic builder stopped at the boundary"))
    }
    fn model(&self) -> String {
        "boundary".into()
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    ws: Workspace,
    worker: Worker,
    seen: Arc<Mutex<Vec<String>>>,
}
impl Fixture {
    fn new(signed_in: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut ws =
            Workspace::create(dir.path(), "Synthetic site", "Rebuild this website", TargetRef::Website { url: "https://example.test/app".into() }).unwrap();
        ws.manifest.stack = "Synthetic stack".into();
        ws.manifest.capture_deep = Some(false);
        ws.save().unwrap();
        std::fs::create_dir_all(ws.capture_dir()).unwrap();
        let png = encode_png(&Image { width: 2, height: 2, rgba: vec![80; 16] }).unwrap();
        std::fs::write(ws.capture_dir().join("screenshot-1440.png"), png).unwrap();
        std::fs::write(ws.capture_dir().join("README.md"), "Synthetic account dashboard").unwrap();
        let metadata = json!({"url":"https://example.test/app", "signed_in":signed_in, "signed_in_site":"site-example.test_443", "sign_in_revision":"fixture-account-1", "sign_in_page":false,
            "screenshots":[{"file":"screenshot-1440.png"}]});
        std::fs::write(ws.capture_dir().join("capture.json"), serde_json::to_vec(&metadata).unwrap()).unwrap();
        let worker = check_worker(&ws, false);
        let profile = craft_anything_engine::tools::site_sign_in::sign_ins_dir(&worker.engine.session()).join("site-example.test_443");
        if signed_in {
            std::fs::create_dir_all(profile.join("profile")).unwrap();
            std::fs::write(profile.join("sign-in.json"), br#"{"version":1,"site":"site-example.test_443","saved_at":1,"revision":"fixture-account-1"}"#)
                .unwrap();
        }
        Self { _dir: dir, ws, worker, seen: Arc::default() }
    }
    fn metadata(&self, f: impl FnOnce(&mut Value)) {
        let path = self.ws.capture_dir().join("capture.json");
        let mut v = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        f(&mut v);
        std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
    }
    fn run(&mut self, on_ask: impl Fn(&Inbox, &Request) + Send + Sync + 'static) -> ToolError {
        let inbox = self.worker.inbox.clone();
        self.worker.wake = Wake::new(move || {
            for r in inbox.pending() {
                on_ask(&inbox, &r);
            }
        });
        let model = BoundaryModel(self.seen.clone(), self.worker.stop.clone(), self.ws.capture_dir());
        let slot = ModelSlot {
            name: "Synthetic".into(),
            model: Box::new(model),
            local: true,
            effort: None,
            max_turns: 1,
            context_tokens: 0,
            tools: Default::default(),
        };
        let api = ApiConversation::start(Arc::new(EngineHost(self.worker.engine.clone())), vec![slot], LoopConfig::default()).unwrap();
        let entry = crate::BackendEntry {
            config: crate::BackendConfig {
                name: "Synthetic".into(),
                kind: BackendKind::OpenaiCompatible,
                model: "boundary".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            },
            options: Default::default(),
        };
        let opts = RunOptions { message: Some("Continue with this capture".into()), max_rounds: 0, ..Default::default() };
        self.worker.run(&mut self.ws, entry, Some(api), None, KeptHistory::default(), &opts).unwrap_err()
    }
}

#[test]
fn rejected_signed_in_capture_never_reaches_the_builder() {
    let mut f = Fixture::new(true);
    f.run(|i, r| i.answer(&r.id, &Answer { allow: Some(false), ..Default::default() }).unwrap());
    assert!(f.seen.lock().unwrap().is_empty(), "rejected account content reached the first builder turn");
}

#[test]
fn signed_in_builder_waits_for_explicit_confirmation() {
    let mut f = Fixture::new(true);
    let engine = f.worker.engine.clone();
    let root = f.ws.root.clone();
    let seen = f.seen.clone();
    let asked = Arc::new(AtomicBool::new(false));
    let asked2 = asked.clone();
    f.run(move |i, r| {
        assert!(seen.lock().unwrap().is_empty(), "the builder ran before the inbox approval");
        asked2.store(true, Ordering::SeqCst);
        let shown = engine.call("project_show", json!({"project":root}), CallOptions::new(Caller::Cli)).unwrap().to_json();
        assert!(shown["result"]["questions"].as_array().unwrap().iter().any(|q| q["id"] == r.id && q["capture_review"].is_object()));
        engine.call("project_inbox_answer", json!({"project":root,"id":r.id,"allow":true}), CallOptions::new(Caller::Cli)).unwrap();
        assert!(i.pending().is_empty());
    });
    assert!(asked.load(Ordering::SeqCst), "signed-in study bypassed the inbox");
    assert_eq!(f.seen.lock().unwrap().len(), 1, "an approved capture reaches the builder");
    assert!(f.seen.lock().unwrap().first().unwrap().ends_with("Synthetic account dashboard"));
}

#[test]
fn unsigned_capture_preserves_the_existing_builder_workflow() {
    let mut f = Fixture::new(false);
    f.run(|_, _| panic!("unsigned capture unexpectedly asked for sign-in confirmation"));
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[test]
fn cancellation_or_an_empty_answer_cannot_release_account_content() {
    for cancel in [true, false] {
        let mut f = Fixture::new(true);
        let stop = f.worker.stop.clone();
        f.run(move |i, r| {
            if cancel {
                stop.cancel();
            } else {
                i.answer(&r.id, &Answer::default()).unwrap();
            }
        });
        assert!(f.seen.lock().unwrap().is_empty());
        assert!(f.worker.inbox.pending().is_empty(), "the stopped request must be withdrawn");
    }
}

#[test]
fn unreadable_or_missing_first_screenshots_stop_before_the_builder() {
    for missing in [true, false] {
        let mut f = Fixture::new(true);
        let file = f.ws.capture_dir().join("screenshot-1440.png");
        if missing {
            std::fs::remove_file(file).unwrap();
        } else {
            std::fs::write(file, "not a PNG").unwrap();
        }
        let error = f.run(|_, _| panic!("invalid pictures must not be offered for approval"));
        assert!(f.seen.lock().unwrap().is_empty());
        assert!(error.remediation.is_some(), "a broken preview needs recovery instructions");
    }
}

#[test]
fn an_expired_sign_in_is_never_offered_as_authenticated_content() {
    let mut f = Fixture::new(true);
    f.metadata(|v| v["sign_in_page"] = json!(true));
    let error = f.run(|_, _| panic!("a password page is not a successful signed-in study"));
    assert!(f.seen.lock().unwrap().is_empty());
    assert!(error.remediation.as_deref().unwrap_or_default().contains("sign"));
}

#[test]
fn changed_capture_or_target_is_not_authorized_by_the_waiting_answer() {
    for target in [true, false] {
        let mut f = Fixture::new(true);
        let root = f.ws.root.clone();
        f.run(move |i, r| {
            let mut ws = Workspace::open(&root).unwrap();
            if target {
                ws.manifest.target = TargetRef::Website { url: "https://other.test/app".into() };
                ws.save().unwrap();
            } else {
                std::fs::write(ws.capture_dir().join("README.md"), "Different account dashboard").unwrap();
            }
            i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap();
        });
        assert!(f.seen.lock().unwrap().is_empty());
    }
}

#[test]
fn changing_the_saved_account_invalidates_a_waiting_confirmation() {
    let mut f = Fixture::new(true);
    let record = craft_anything_engine::tools::site_sign_in::sign_ins_dir(&f.worker.engine.session()).join("site-example.test_443/sign-in.json");
    f.run(move |i, r| {
        std::fs::write(&record, br#"{"version":1,"site":"site-example.test_443","saved_at":2,"revision":"fixture-account-2"}"#).unwrap();
        i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap();
    });
    assert!(f.seen.lock().unwrap().is_empty());
}

#[test]
fn a_resumed_run_does_not_reuse_a_previous_confirmation() {
    let mut f = Fixture::new(true);
    f.run(|i, r| i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap());
    let previous = f.seen.lock().unwrap().len();
    f.worker.stop = CancelToken::default();
    f.worker.seen.clear();
    let asked = Arc::new(AtomicBool::new(false));
    let asked2 = asked.clone();
    f.run(move |i, r| {
        asked2.store(true, Ordering::SeqCst);
        i.answer(&r.id, &Answer { allow: Some(false), ..Default::default() }).unwrap();
    });
    assert!(asked.load(Ordering::SeqCst), "resume must request fresh consent");
    assert_eq!(f.seen.lock().unwrap().len(), previous);
}

#[test]
fn an_unavailable_inbox_stops_before_the_builder() {
    let mut f = Fixture::new(true);
    std::fs::write(f.ws.inbox_dir(), "not a directory").unwrap();
    f.run(|_, _| panic!("the unavailable inbox cannot post an approval"));
    assert!(f.seen.lock().unwrap().is_empty());
}

#[test]
fn screenshot_paths_cannot_escape_project_storage() {
    for file in ["../outside.png", "/outside.png", "screenshot-../outside.png", "screenshot-https:outside.png"] {
        let mut f = Fixture::new(true);
        f.metadata(|v| v["screenshots"][0]["file"] = json!(file));
        f.run(|_, _| panic!("an invalid path must not be previewed"));
        assert!(f.seen.lock().unwrap().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn screenshot_symlinks_cannot_escape_project_storage() {
    use std::os::unix::fs::symlink;
    let mut f = Fixture::new(true);
    let file = f.ws.capture_dir().join("screenshot-1440.png");
    let outside = f.ws.root.join("outside.png");
    std::fs::rename(&file, &outside).unwrap();
    symlink(outside, file).unwrap();
    f.run(|_, _| panic!("a symlink must not be previewed"));
    assert!(f.seen.lock().unwrap().is_empty());
}

#[test]
fn restart_replaces_abandoned_capture_requests_without_reusing_their_answers() {
    use craft_anything_engine::capture_review::CaptureReview;
    let mut f = Fixture::new(true);
    let old = f
        .worker
        .inbox
        .post_capture_review(CaptureReview { website: "https://old.test".into(), capture_sha256: "old-capture".into(), screenshots: Vec::new() })
        .unwrap();
    let old2 = old.clone();
    f.run(move |i, r| {
        assert_ne!(r.id, old2, "the abandoned approval must not be presented again");
        assert_eq!(i.pending().len(), 1);
        i.answer(&old2, &Answer { allow: Some(true), ..Default::default() }).unwrap();
        i.answer(&r.id, &Answer { allow: Some(false), ..Default::default() }).unwrap();
    });
    assert!(f.seen.lock().unwrap().is_empty(), "the old answer cannot release the new capture");
    assert!(!f.worker.inbox.contains_request(&old));
}

#[test]
fn missing_authentication_metadata_cannot_turn_a_private_capture_into_an_unsigned_one() {
    for missing in [true, false] {
        let mut f = Fixture::new(true);
        std::fs::remove_dir_all(craft_anything_engine::tools::site_sign_in::sign_ins_dir(&f.worker.engine.session())).unwrap();
        if missing {
            std::fs::remove_file(f.ws.capture_dir().join("capture.json")).unwrap();
        } else {
            f.metadata(|v| {
                v.as_object_mut().unwrap().remove("signed_in");
            });
        }
        f.run(|_, _| panic!("unknown authentication state cannot be offered as a valid capture"));
        assert!(f.seen.lock().unwrap().is_empty());
    }
}

// Review #6004691567: original synthetic fixture extended with local route captures only.
impl Fixture {
    fn save_account(&self, revision: &str) {
        let dir = craft_anything_engine::tools::site_sign_in::sign_ins_dir(&self.worker.engine.session()).join("site-example.test_443");
        std::fs::create_dir_all(dir.join("profile")).unwrap();
        std::fs::write(dir.join("sign-in.json"), serde_json::to_vec(&json!({"version":1,"site":"site-example.test_443","revision":revision})).unwrap())
            .unwrap();
    }

    fn route(&self, name: &str, signed_in: bool, revision: &str) {
        let dir = self.ws.capture_dir().join("routes").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let png = encode_png(&Image { width: 2, height: 2, rgba: vec![120; 16] }).unwrap();
        std::fs::write(dir.join("screenshot-1440.png"), png).unwrap();
        std::fs::write(dir.join("README.md"), if signed_in { "Synthetic private route content" } else { "Synthetic public route content" }).unwrap();
        let record = json!({"url":format!("https://example.test/{name}"),"signed_in":signed_in,"signed_in_site":"site-example.test_443",
            "sign_in_revision":revision,"sign_in_page":false,"screenshots":[{"file":"screenshot-1440.png"}]});
        std::fs::write(dir.join("capture.json"), serde_json::to_vec(&record).unwrap()).unwrap();
        let index = self.ws.capture_dir().join("routes.json");
        let mut routes: Vec<Value> = std::fs::read(&index).ok().map(|b| serde_json::from_slice(&b).unwrap()).unwrap_or_default();
        routes.push(json!({"url":format!("https://example.test/{name}"),"path":format!("/{name}"),"slug":name}));
        std::fs::write(index, serde_json::to_vec(&routes).unwrap()).unwrap();
    }

    /// A retry goes through the real worker's Study branch, stopping at the unavailable test
    /// browser. testing::session finds no installed engines, so this never opens a browser/network.
    fn assert_invalidated_and_restudied(&mut self) {
        let saved = Workspace::open(&self.ws.root).unwrap();
        assert_eq!(saved.manifest.capture_deep, None, "invalid evidence remained reusable on disk");
        self.ws = saved;
        self.worker.stop = CancelToken::default();
        self.worker.seen.clear();
        let (tx, rx) = std::sync::mpsc::channel();
        self.worker.tx = tx;
        let error = self.run(|_, _| panic!("retry reused the invalid capture instead of studying"));
        assert_eq!(error.code, ErrorCode::EngineMissing, "retry should reach the fresh study's browser preflight: {error:?}");
        assert!(rx.try_iter().any(|e| matches!(e, RunEvent::Agent(AgentEvent::ToolStarted { tool, .. }) if tool == "web_site_open")));
        assert!(self.seen.lock().unwrap().is_empty());
    }
}

#[test]
fn review_fix_initial_validation_failure_invalidates_and_retries_the_study() {
    for failure in ["missing", "invalid", "account"] {
        let mut f = Fixture::new(true);
        std::fs::create_dir_all(f.ws.app_dir()).unwrap();
        std::fs::write(f.ws.app_dir().join("keep.txt"), "unrelated work").unwrap();
        let shot = f.ws.capture_dir().join("screenshot-1440.png");
        match failure {
            "missing" => std::fs::remove_file(shot).unwrap(),
            "invalid" => std::fs::write(shot, "damaged PNG").unwrap(),
            _ => f.save_account("replacement-account"),
        }
        f.run(|_, _| panic!("invalid capture must not ask for approval"));
        assert!(f.seen.lock().unwrap().is_empty());
        f.assert_invalidated_and_restudied();
        assert_eq!(std::fs::read_to_string(f.ws.app_dir().join("keep.txt")).unwrap(), "unrelated work");
    }
}

#[test]
fn review_fix_post_approval_failure_invalidates_and_retries_the_study() {
    for failure in ["account", "bytes", "missing", "invalid"] {
        let mut f = Fixture::new(true);
        let root = f.ws.root.clone();
        let config = craft_anything_engine::tools::site_sign_in::sign_ins_dir(&f.worker.engine.session());
        f.run(move |i, r| {
            match failure {
                "account" => std::fs::write(
                    config.join("site-example.test_443/sign-in.json"),
                    br#"{"version":1,"site":"site-example.test_443","revision":"replacement"}"#,
                )
                .unwrap(),
                "bytes" => std::fs::write(root.join("capture/README.md"), "changed capture").unwrap(),
                "missing" => std::fs::remove_file(root.join("capture/screenshot-1440.png")).unwrap(),
                _ => std::fs::write(root.join("capture/screenshot-1440.png"), "damaged PNG").unwrap(),
            }
            i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap();
        });
        assert!(f.seen.lock().unwrap().is_empty());
        f.assert_invalidated_and_restudied();
    }
}

#[test]
fn review_fix_unchanged_approval_and_cancellation_preserve_usable_captures() {
    for cancel in [false, true] {
        let mut f = Fixture::new(true);
        let stop = f.worker.stop.clone();
        let original = std::fs::read(f.ws.capture_dir().join("capture.json")).unwrap();
        f.run(move |i, r| {
            if cancel {
                stop.cancel();
            } else {
                i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap();
            }
        });
        assert_eq!(Workspace::open(&f.ws.root).unwrap().manifest.capture_deep, Some(false));
        assert_eq!(std::fs::read(f.ws.capture_dir().join("capture.json")).unwrap(), original);
        assert_eq!(f.seen.lock().unwrap().len(), usize::from(!cancel));
    }
}

#[test]
fn review_fix_unsigned_root_with_private_route_waits_for_the_right_screenshots() {
    for allow in [false, true] {
        let mut f = Fixture::new(false);
        f.save_account("fixture-account-1");
        f.route("account", true, "fixture-account-1");
        let root = f.ws.root.clone();
        let seen = f.seen.clone();
        let asked = Arc::new(AtomicBool::new(false));
        let asked2 = asked.clone();
        f.run(move |i, r| {
            assert!(seen.lock().unwrap().is_empty(), "private route reached the builder before confirmation");
            let review = r.capture_review.as_ref().unwrap();
            assert!(!review.screenshots.is_empty());
            assert!(
                review.screenshots.iter().all(|s| s.file.starts_with("routes/account/")),
                "unsigned root screenshots cannot stand in for private-route review"
            );
            for shot in &review.screenshots {
                assert!(review.preview_bytes(&root, shot).is_ok());
            }
            asked2.store(true, Ordering::SeqCst);
            i.answer(&r.id, &Answer { allow: Some(allow), ..Default::default() }).unwrap();
        });
        assert!(asked.load(Ordering::SeqCst), "private route bypassed confirmation");
        assert_eq!(f.seen.lock().unwrap().len(), usize::from(allow));
        if allow {
            assert!(f.seen.lock().unwrap()[0].contains("Synthetic private route content"));
        }
    }
}

#[test]
fn review_fix_mixed_route_accounts_fail_closed() {
    let mut f = Fixture::new(true);
    f.route("account", true, "another-account");
    f.run(|_, _| panic!("mixed accounts cannot be authorized by a root-page preview"));
    assert!(f.seen.lock().unwrap().is_empty());
    f.assert_invalidated_and_restudied();
}

#[test]
fn review_fix_consistent_signed_in_pages_are_reviewed_together() {
    let mut f = Fixture::new(true);
    f.route("account", true, "fixture-account-1");
    f.route("settings", true, "fixture-account-1");
    let root = f.ws.root.clone();
    let seen = f.seen.clone();
    f.run(move |i, r| {
        assert!(seen.lock().unwrap().is_empty());
        let review = r.capture_review.as_ref().unwrap();
        for dir in ["screenshot-1440.png", "routes/account/screenshot-1440.png", "routes/settings/screenshot-1440.png"] {
            let shot = review.screenshots.iter().find(|s| s.file == dir).expect("every signed-in page needs its preview");
            assert!(review.preview_bytes(&root, shot).is_ok());
        }
        i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap();
    });
    assert_eq!(f.seen.lock().unwrap().len(), 1);
    assert_eq!(Workspace::open(&f.ws.root).unwrap().manifest.capture_deep, Some(false));
}

#[test]
fn review_fix_fully_unsigned_routes_keep_the_unsigned_workflow() {
    let mut f = Fixture::new(false);
    f.route("account", false, "unused");
    // Public redirects do not establish or require a saved-account identity.
    f.metadata(|v| v["url"] = json!("https://public.example.test/landing"));
    f.run(|_, _| panic!("fully unsigned study must not require private-content approval"));
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[test]
fn review_fix_ambiguous_nested_metadata_fails_closed() {
    for failure in ["missing", "invalid", "unknown", "unsafe-index", "missing-route", "unsafe-shot", "foreign-site"] {
        let mut f = Fixture::new(true);
        f.route("account", true, "fixture-account-1");
        let record = f.ws.capture_dir().join("routes/account/capture.json");
        match failure {
            "missing" => std::fs::remove_file(record).unwrap(),
            "invalid" => std::fs::write(record, "not JSON").unwrap(),
            "unsafe-index" => std::fs::write(f.ws.capture_dir().join("routes.json"), br#"[{"slug":"../outside"}]"#).unwrap(),
            "missing-route" => {
                std::fs::write(f.ws.capture_dir().join("routes.json"), br#"[{"slug":"not-captured","url":"https://example.test/not-captured"}]"#).unwrap()
            }
            _ => {
                let mut v: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
                match failure {
                    "unknown" => {
                        v.as_object_mut().unwrap().remove("signed_in");
                    }
                    "unsafe-shot" => v["screenshots"][0]["file"] = json!("../screenshot-1440.png"),
                    _ => v["signed_in_site"] = json!("site-other.test_443"),
                }
                std::fs::write(record, serde_json::to_vec(&v).unwrap()).unwrap();
            }
        }
        f.run(|_, _| panic!("ambiguous nested content must not be offered for approval"));
        assert!(f.seen.lock().unwrap().is_empty());
        f.assert_invalidated_and_restudied();
    }
}

#[test]
fn review_fix_changed_private_route_while_pending_cannot_reach_the_builder() {
    let mut f = Fixture::new(false);
    f.save_account("fixture-account-1");
    f.route("account", true, "fixture-account-1");
    let route = f.ws.capture_dir().join("routes/account/README.md");
    let asked = Arc::new(AtomicBool::new(false));
    let asked2 = asked.clone();
    f.run(move |i, r| {
        asked2.store(true, Ordering::SeqCst);
        std::fs::write(&route, "different private route").unwrap();
        i.answer(&r.id, &Answer { allow: Some(true), ..Default::default() }).unwrap();
    });
    assert!(asked.load(Ordering::SeqCst));
    assert!(f.seen.lock().unwrap().is_empty());
    f.assert_invalidated_and_restudied();
}

#[test]
fn review_fix_unindexed_private_route_is_still_reviewed() {
    let mut f = Fixture::new(false);
    f.save_account("fixture-account-1");
    f.route("account", true, "fixture-account-1");
    std::fs::remove_file(f.ws.capture_dir().join("routes.json")).unwrap();
    let asked = Arc::new(AtomicBool::new(false));
    let asked2 = asked.clone();
    f.run(move |i, r| {
        asked2.store(true, Ordering::SeqCst);
        assert!(r.capture_review.as_ref().unwrap().screenshots.iter().all(|s| s.file.starts_with("routes/account/")));
        i.answer(&r.id, &Answer { allow: Some(false), ..Default::default() }).unwrap();
    });
    assert!(asked.load(Ordering::SeqCst));
    assert!(f.seen.lock().unwrap().is_empty());
}
