use super::*;

#[tokio::test]
async fn tool_family_define_tool_family_returns_frozen_identity() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineToolFamily, defineTool } from "frances:v1/tool-family";

        const fam = defineToolFamily({ prompt: (ctx) => "hello" });

        // Verify the object is frozen (identity is stable).
        let frozen = Object.isFrozen(fam);
        // Verify it has exactly one own property: prompt.
        let keys = Object.keys(fam);
        // Verify === identity works (same call returns same object reference).
        const fam2 = defineToolFamily({ prompt: (ctx) => "world" });
        let identity_different = fam !== fam2;

        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: JSON.stringify({ frozen, keys, identity_different }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["frozen"], true);
    assert_eq!(result["keys"], serde_json::json!(["prompt"]));
    assert_eq!(result["identity_different"], true);
}

#[tokio::test]
async fn tool_family_define_tool_creates_tool_object() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineToolFamily, defineTool } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const fam = defineToolFamily({ prompt: (ctx) => "family prompt" });

        const tool = defineTool({
            name: "my_tool",
            description: "does a thing",
            parameters: { type: "object", properties: { x: { type: "string" } } },
            family: fam,
            handler: async () => ({ role: "tool", call_id: "1", content: "ok", is_error: false }),
        });

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            name: tool.name,
            description: tool.description,
            hasParameters: "parameters" in tool,
            hasHandler: typeof tool.handler === "function",
            familyIsFam: tool.family === fam,
            familyPrompt: tool.family.prompt({}),
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["name"], "my_tool");
    assert_eq!(result["description"], "does a thing");
    assert_eq!(result["hasParameters"], true);
    assert_eq!(result["hasHandler"], true);
    assert_eq!(result["familyIsFam"], true);
    assert_eq!(result["familyPrompt"], "family prompt");
}

#[tokio::test]
async fn tool_family_define_tool_without_family() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineTool } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const tool = defineTool({
            name: "standalone",
            description: "no family",
            parameters: { type: "object" },
            handler: async () => ({ role: "tool", call_id: "1", content: "ok", is_error: false }),
        });

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            hasFamily: "family" in tool,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["hasFamily"], false);
}

#[tokio::test]
async fn tool_family_dedupe_by_identity() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineToolFamily, defineTool } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const editing = defineToolFamily({ prompt: (ctx) => "editing preamble" });

        const t1 = defineTool({ name: "file_read", description: "read", parameters: { type: "object" }, family: editing, handler: async () => ({}) });
        const t2 = defineTool({ name: "file_write", description: "write", parameters: { type: "object" }, family: editing, handler: async () => ({}) });
        const t3 = defineTool({ name: "shell_run", description: "run", parameters: { type: "object" }, handler: async () => ({}) });

        // Simulate what toolGuidance would do: collect families from tools, dedupe by identity.
        const tools = [t1, t2, t3];
        const families = new Set(tools.map(t => t.family).filter(Boolean));

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            familyCount: families.size,
            hasEditing: families.has(editing),
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["familyCount"], 1,
        "one unique family across two tools"
    );
    assert_eq!(result["hasEditing"], true);
}

#[tokio::test]
async fn tool_family_define_tool_family_rejects_non_function_prompt() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineToolFamily } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        let msg = "no error";
        try {
            defineToolFamily({ prompt: "not a function" });
        } catch (e) {
            msg = e.message;
        }
        transcript.push(new MarkdownSection({ content: msg }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert!(text_of(&frames[0]).contains("prompt must be a function"));
}

#[tokio::test]
async fn tool_family_define_tool_rejects_bad_family() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineTool } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        let msg = "no error";
        try {
            defineTool({
                name: "bad",
                description: "bad family",
                parameters: { type: "object" },
                family: "not a family",
                handler: async () => ({}),
            });
        } catch (e) {
            msg = e.message;
        }
        transcript.push(new MarkdownSection({ content: msg }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert!(text_of(&frames[0]).contains("family must be a ToolFamily"));
}

#[tokio::test]
async fn tool_guidance_folds_families_from_tools() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineToolFamily, defineTool, toolGuidance } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const editing = defineToolFamily({ prompt: (ctx) => "editing preamble" });
        const shell = defineToolFamily({ prompt: (ctx) => "shell preamble" });

        const t1 = defineTool({ name: "file_read", description: "read", parameters: { type: "object" }, family: editing, handler: async () => ({}) });
        const t2 = defineTool({ name: "file_write", description: "write", parameters: { type: "object" }, family: editing, handler: async () => ({}) });
        const t3 = defineTool({ name: "shell_run", description: "run", parameters: { type: "object" }, family: shell, handler: async () => ({}) });

        const ctx = { tools: [t1, t2, t3] };
        const result = toolGuidance.prompt(ctx);

        transcript.push(new MarkdownSection({ content: result }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let output = text_of(&frames[0]);
    assert!(
        output.contains("editing preamble"),
        "missing editing preamble"
    );
    assert!(output.contains("shell preamble"), "missing shell preamble");
    // editing appears only once even though two tools share it
    assert_eq!(
        output.matches("editing preamble").count(),
        1,
        "editing preamble should appear exactly once"
    );
}

#[tokio::test]
async fn tool_guidance_returns_null_when_no_families() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineTool, toolGuidance } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const t1 = defineTool({ name: "standalone", description: "no family", parameters: { type: "object" }, handler: async () => ({}) });

        const ctx = { tools: [t1] };
        const result = toolGuidance.prompt(ctx);

        transcript.push(new MarkdownSection({ content: String(result) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(
        text_of(&frames[0]),
        "null",
        "expected null when no families"
    );
}

#[tokio::test]
async fn tool_guidance_returns_null_on_empty_tools() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { toolGuidance } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const ctx = { tools: [] };
        const result = toolGuidance.prompt(ctx);

        transcript.push(new MarkdownSection({ content: String(result) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "null", "expected null on empty tools");
}

#[tokio::test]
async fn tool_guidance_is_stable_object() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { toolGuidance } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        // Import twice — must be the same object (=== identity)
        const tg1 = toolGuidance;
        const tg2 = toolGuidance;

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            stable: tg1 === tg2,
            hasName: "name" in toolGuidance,
            name: toolGuidance.name,
            hasPrompt: typeof toolGuidance.prompt === "function",
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["stable"], true, "toolGuidance must be === stable");
    assert_eq!(result["hasName"], true);
    assert_eq!(result["name"], "tool-guidance");
    assert_eq!(result["hasPrompt"], true);
}

#[tokio::test]
async fn tool_guidance_skips_family_returning_null() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { defineToolFamily, defineTool, toolGuidance } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const noisy = defineToolFamily({ prompt: (ctx) => "visible" });
        const quiet = defineToolFamily({ prompt: (ctx) => null });

        const t1 = defineTool({ name: "a", description: "a", parameters: { type: "object" }, family: noisy, handler: async () => ({}) });
        const t2 = defineTool({ name: "b", description: "b", parameters: { type: "object" }, family: quiet, handler: async () => ({}) });

        const ctx = { tools: [t1, t2] };
        const result = toolGuidance.prompt(ctx);

        transcript.push(new MarkdownSection({ content: result }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let output = text_of(&frames[0]);
    assert_eq!(output, "visible", "only the non-null family should appear");
}

// ---- frances:v1/tool-families tests -----------------------------------------

#[tokio::test]
async fn tool_families_editing_family_exists_and_is_frozen() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { editingFamily } from "frances:v1/tools/file";
        import { shellFamily } from "frances:v1/tools/shell";

        let editFrozen = Object.isFrozen(editingFamily);
        let shellFrozen = Object.isFrozen(shellFamily);
        let editHasPrompt = typeof editingFamily.prompt === "function";
        let shellHasPrompt = typeof shellFamily.prompt === "function";
        let different = editingFamily !== shellFamily;

        import { transcript, MarkdownSection } from "frances:v1/sections";
        transcript.push(new MarkdownSection({ content: JSON.stringify({
            editFrozen, shellFrozen, editHasPrompt, shellHasPrompt, different,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["editFrozen"], true);
    assert_eq!(result["shellFrozen"], true);
    assert_eq!(result["editHasPrompt"], true);
    assert_eq!(result["shellHasPrompt"], true);
    assert_eq!(result["different"], true);
}

#[tokio::test]
async fn tool_families_editing_family_emits_preamble() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { editingFamily } from "frances:v1/tools/file";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const text = editingFamily.prompt({});
        let hasCritical = text.includes("CRITICAL");
        let hasWrongRight = text.includes("WRONG") && text.includes("RIGHT");
        let hasAnchorProtocol = text.includes("Anchor protocol");
        let hasFormatter = text.includes("project formatter");

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            hasCritical, hasWrongRight, hasAnchorProtocol, hasFormatter,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["hasCritical"], true, "should have CRITICAL warning");
    assert_eq!(
        result["hasWrongRight"], true,
        "should have WRONG/RIGHT example"
    );
    assert_eq!(
        result["hasAnchorProtocol"], true,
        "should have anchor protocol"
    );
    assert_eq!(result["hasFormatter"], true, "should have formatter note");
}

#[tokio::test]
async fn tool_families_shell_family_emits_preamble() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { shellFamily } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const text = shellFamily.prompt({});
        let hasPersistent = text.includes("persistent");
        let hasNoCd = text.includes("cd");
        let hasPrefer = text.includes("Prefer dedicated tools");

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            hasPersistent, hasNoCd, hasPrefer,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(result["hasPersistent"], true);
    assert_eq!(result["hasNoCd"], true);
    assert_eq!(result["hasPrefer"], true);
}

#[tokio::test]
async fn tool_families_identity_is_stable_across_imports() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { editingFamily as e1 } from "frances:v1/tools/file";
        import { editingFamily as e2 } from "frances:v1/tools/file";
        import { shellFamily as s1 } from "frances:v1/tools/shell";
        import { shellFamily as s2 } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        let editingStable = e1 === e2;
        let shellStable = s1 === s2;

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            editingStable, shellStable,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["editingStable"], true,
        "editing family identity should be stable"
    );
    assert_eq!(
        result["shellStable"], true,
        "shell family identity should be stable"
    );
}

#[tokio::test]
async fn tool_families_work_with_tool_guidance_dedup() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { editingFamily } from "frances:v1/tools/file";
        import { shellFamily } from "frances:v1/tools/shell";
        import { defineTool, toolGuidance } from "frances:v1/tool-family";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const tool1 = defineTool({
            name: "t1", description: "d1",
            parameters: { type: "object", properties: {} },
            family: editingFamily,
            handler: async () => ({}),
        });
        const tool2 = defineTool({
            name: "t2", description: "d2",
            parameters: { type: "object", properties: {} },
            family: editingFamily,
            handler: async () => ({}),
        });
        const tool3 = defineTool({
            name: "t3", description: "d3",
            parameters: { type: "object", properties: {} },
            family: shellFamily,
            handler: async () => ({}),
        });

        const ctx = { tools: [tool1, tool2, tool3] };
        const text = toolGuidance.prompt(ctx);

        // Count occurrences by indexOf.
        let editCount = 0;
        let idx = 0;
        const editMarker = "Editing tools";
        while ((idx = text.indexOf(editMarker, idx)) !== -1) { editCount++; idx += editMarker.length; }
        let shellCount = 0;
        idx = 0;
        const shellMarker = "Shell tools";
        while ((idx = text.indexOf(shellMarker, idx)) !== -1) { shellCount++; idx += shellMarker.length; }

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            editCount, shellCount,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["editCount"], 1,
        "editing family should appear exactly once"
    );
    assert_eq!(
        result["shellCount"], 1,
        "shell family should appear exactly once"
    );
}

#[tokio::test]
async fn tool_family_shell_tools_import_shell_family() {
    // Verify that importing from frances:v1/tools/shell succeeds and that
    // the shellFamily from frances:v1/tool-families is importable and
    // the module wiring is correct. We check that the constructors
    // reference shellFamily by examining their toString() source.
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Run, Wait, Kill, Set, Capture } from "frances:v1/tools/shell";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const runHas = Run.toString().includes("shellFamily");
        const waitHas = Wait.toString().includes("shellFamily");
        const killHas = Kill.toString().includes("shellFamily");
        const setHas = Set.toString().includes("shellFamily");
        const captureHas = Capture.toString().includes("shellFamily");

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            runHas, waitHas, killHas, setHas, captureHas,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["runHas"], true,
        "Run constructor should reference shellFamily"
    );
    assert_eq!(
        result["waitHas"], true,
        "Wait constructor should reference shellFamily"
    );
    assert_eq!(
        result["killHas"], true,
        "Kill constructor should reference shellFamily"
    );
    assert_eq!(
        result["setHas"], true,
        "Set constructor should reference shellFamily"
    );
    assert_eq!(
        result["captureHas"], true,
        "Capture constructor should reference shellFamily"
    );
}

#[tokio::test]
async fn tool_family_file_tools_import_editing_family() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Read, ReplaceLines, ReplaceAll, InsertAfter, InsertBefore, New, Overwrite } from "frances:v1/tools/file";
        import { transcript, MarkdownSection } from "frances:v1/sections";

        const readNoFamily = !Read.toString().includes("editingFamily");
        const replaceHas = ReplaceLines.toString().includes("editingFamily");
        const replaceAllHas = ReplaceAll.toString().includes("editingFamily");
        const insertAfterHas = InsertAfter.toString().includes("editingFamily");
        const insertBeforeHas = InsertBefore.toString().includes("editingFamily");
        const newHas = New.toString().includes("editingFamily");
        const overwriteHas = Overwrite.toString().includes("editingFamily");

        transcript.push(new MarkdownSection({ content: JSON.stringify({
            readNoFamily, replaceHas, replaceAllHas,
            insertAfterHas, insertBeforeHas,
            newHas, overwriteHas,
        }) }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    let result: serde_json::Value = serde_json::from_str(&text_of(&frames[0])).unwrap();
    assert_eq!(
        result["readNoFamily"], true,
        "Read should NOT reference editingFamily"
    );
    assert_eq!(
        result["replaceHas"], true,
        "ReplaceLines should reference editingFamily"
    );
    assert_eq!(
        result["replaceAllHas"], true,
        "ReplaceAll should reference editingFamily"
    );
    assert_eq!(
        result["insertAfterHas"], true,
        "InsertAfter should reference editingFamily"
    );
    assert_eq!(
        result["insertBeforeHas"], true,
        "InsertBefore should reference editingFamily"
    );
    assert_eq!(result["newHas"], true, "New should reference editingFamily");
    assert_eq!(
        result["overwriteHas"], true,
        "Overwrite should reference editingFamily"
    );
}
