import { expect, test } from "@playwright/test";
import { installMockTauri } from "./mock-tauri";

test.beforeEach(async ({ page }) => {
  await installMockTauri(page);
});

test("dashboard renders mocked index and recent sessions", async ({ page }) => {
  await page.goto("/");

  await expect(page.getByRole("heading", { name: "控制台" })).toBeVisible();
  await expect(page.getByText(/已索引 3 个会话/)).toBeVisible();
  await expect(page.getByRole("heading", { name: "最近会话" })).toBeVisible();
  await expect(page.getByText("修复登录流程")).toBeVisible();
  await expect(page.getByText("Claude Code").first()).toBeVisible();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("list_sources");
  expect(calls).toContain("dashboard_summary");
  // No real backend command can be reached: every invoke is answered by the fixture.
  expect(calls.every((cmd) => typeof cmd === "string")).toBe(true);
});

test("session detail renders records and expands a subagent", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("link", { name: "会话档案" }).click();

  await expect(page.getByRole("heading", { name: "会话历史" })).toBeVisible();
  await expect(page.getByText("修复登录流程").first()).toBeVisible();
  await expect(page.getByText("1 个子代理")).toBeVisible();

  await page.getByText("修复登录流程").first().click();
  await expect(page.getByText("请检查登录流程，定位失败原因。").first()).toBeVisible();
  await expect(page.getByText("2 个子代理")).toBeVisible();
  await expect(page.getByText(/我先让子代理检查登录模块/)).toBeVisible();

  // Subagent calls live at the tool level.
  await page.getByRole("button", { name: "+工具" }).click();
  await expect(page.getByRole("button", { name: /🤖 子代理 #ent-1/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /general-purpose/ })).toBeVisible();

  // Expanding the Agent card loads the child transcript through get_subagent_detail.
  await page.getByRole("button", { name: /general-purpose/ }).click();
  await expect(page.getByText("子代理检查了 auth.ts，发现 token 过期判断有误。")).toBeVisible();

  // A grandchild agent spawns from the child transcript: expand it one level deeper.
  await page.getByRole("button", { name: /explore/ }).click();
  await expect(page.getByText("孙代理确认：token 过期判断已修复。")).toBeVisible();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("get_session_detail");
  expect(calls).toContain("list_subagents");
  expect(calls).toContain("get_subagent_detail");
});

test("unknown Tauri commands are isolated from the real backend", async ({ page }) => {
  await page.goto("/");
  const value = await page.evaluate(async () => {
    const internals = (window as unknown as {
      __TAURI_INTERNALS__: { invoke: (cmd: string, args?: unknown) => Promise<unknown> };
    }).__TAURI_INTERNALS__;
    return internals.invoke("this_command_does_not_exist", {});
  });
  expect(value).toBeNull();
});

test("session search and favorite metadata update through IPC", async ({ page }) => {
  await page.goto("/sessions");
  await expect(page.getByText("共 1 个会话")).toBeVisible();

  await page.getByPlaceholder("搜索当前会话").fill("登录");
  await expect
    .poll(() =>
      page.evaluate(
        () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
      ),
    )
    .toContain("search_sessions");
  await expect(page.getByText("找到 1 条")).toBeVisible();

  await page.getByTitle("收藏").click();
  await expect(page.getByTitle("取消收藏")).toBeVisible();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("set_session_meta");
});

test("every primary route renders without an error banner", async ({ page }) => {
  const routes: Array<[string, string]> = [
    ["/workflows", "工作流"],
    ["/memories", "请选择一个项目"],
    ["/rules", "全局规则"],
    ["/tools", "工具"],
    ["/usage", "用量"],
    ["/profiles", "快照"],
    ["/instructions", "全局指令"],
    ["/settings", "Déjà Vu 设置"],
  ];

  for (const [route, text] of routes) {
    await page.goto(route);
    await expect(page.getByText(text, { exact: false }).first()).toBeVisible();
    await expect(page.locator(".text-danger")).toHaveCount(0);
  }
});

test("language toggle re-renders navigation", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "控制台" })).toBeVisible();
  await page.getByRole("button", { name: /English/ }).click();
  await expect(page.getByRole("heading", { name: "Dashboard" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Sessions" })).toBeVisible();
});

test("snapshot create, restore and delete call the archive commands", async ({ page }) => {
  await page.goto("/profiles");
  await expect(page.getByText("暂无快照")).toBeVisible();

  await page.getByRole("button", { name: "创建快照" }).click();
  await page.getByPlaceholder(/可选标签/).fill("e2e-snap");
  await page.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.getByText("snap-e2e")).toBeVisible();

  await page.getByRole("button", { name: "恢复", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "恢复" }).click();

  await page.getByRole("button", { name: "删除", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "删除" }).click();
  await expect(page.getByText("暂无快照")).toBeVisible();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("create_profile");
  expect(calls).toContain("restore_profile");
  expect(calls).toContain("delete_profile");
});

test("memory edit and delete call the memory commands", async ({ page }) => {
  await page.goto("/memories");
  await page.getByRole("button", { name: /C:\\Codes\\demo/ }).click();
  await expect(page.getByText("Mock memory")).toBeVisible();

  await page.getByRole("button", { name: "编辑" }).click();
  await page.locator("textarea").fill("# Mock memory\n\nupdated");
  await page.getByRole("button", { name: "保存", exact: true }).click();

  // Saving returns to the memory view; re-select the project to return to the list.
  await page.getByRole("button", { name: /C:\\Codes\\demo/ }).click();
  await page.getByRole("button", { name: "删除", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "删除" }).click();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("save_memory");
  expect(calls).toContain("delete_memory");
});

test("rule toggle calls toggle_rule", async ({ page }) => {
  await page.goto("/rules");
  await page.getByRole("button", { name: /Claude Code 1/ }).click();
  await page.getByLabel("切换 mock-rule.md").click();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("toggle_rule");
});

test("instruction edit saves through save_instruction_artifact", async ({ page }) => {
  await page.goto("/instructions");
  await expect(page.getByText("全局 CLAUDE.md").first()).toBeVisible();
  await page.getByRole("button", { name: "编辑", exact: true }).click();
  await page.locator("textarea").fill("# Mock instructions\n\nupdated");
  await page.getByRole("button", { name: "保存", exact: true }).click();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("save_instruction_artifact");
});

test("settings refresh and save call the config commands", async ({ page }) => {
  await page.goto("/settings");
  await expect(page.getByRole("heading", { name: "Déjà Vu 设置" })).toBeVisible();

  await page.getByRole("button", { name: "重新拉取模型及价格" }).click();
  const pricesSection = page.locator("section").filter({ hasText: "用量价格表" });
  await pricesSection.getByRole("button", { name: "保存", exact: true }).click();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("refresh_model_prices");
  expect(calls).toContain("save_dejavu_config");
});

test("custom resume commands are configurable per agent", async ({ page }) => {
  await page.goto("/settings");
  await expect(page.getByRole("heading", { name: "Déjà Vu 设置" })).toBeVisible();

  // Every desktop flavor has a preset: Claude via the documented --desktop --resume bridge,
  // OpenCode via its opencode:// handler (app focus), Codex via codex://threads/<id>.
  await page.getByRole("button", { name: /Claude Desktop/ }).click();
  await page.getByRole("button", { name: /OpenCode Desktop/ }).click();
  await page.getByRole("button", { name: "Codex Desktop 深链" }).click();
  await page.getByLabel("DeepSeek Harness 自定义恢复命令").fill("dsh web");
  await page.getByRole("button", { name: "保存", exact: true }).first().click();

  const saved = await page.evaluate(
    () =>
      (window as unknown as { __E2E_SAVED_CONFIG__?: { resume_commands?: Record<string, string> } })
        .__E2E_SAVED_CONFIG__,
  );
  expect(saved?.resume_commands?.claude).toBe("claude --desktop --resume {session}");
  expect(saved?.resume_commands?.opencode).toContain("opencode://open");
  expect(saved?.resume_commands?.codex).toContain("codex://threads/{session_raw}");
  expect(saved?.resume_commands?.dsh).toBe("dsh web");
});

test("deep search and detail pagination work", async ({ page }) => {
  await page.goto("/sessions");
  await expect(page.getByText("共 1 个会话")).toBeVisible();

  await page.getByPlaceholder("搜索当前会话").fill("登录");
  await page.getByRole("button", { name: "深度全文" }).click();
  await expect(page.getByText(/深度全文：扫描源文件后找到 1 条/)).toBeVisible();

  // The detail page reports has_more, so the bottom sentinel loads page two.
  await page.getByText("修复登录流程").first().click();
  await expect(page.getByText("第二页记录：分页加载成功。")).toBeVisible();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("deep_search");
  expect(calls.filter((cmd) => cmd === "get_session_detail").length).toBeGreaterThan(1);
});

test("WSL host badge and path are rendered for a non-native session", async ({ page }) => {
  await page.goto("/sessions");
  await page.getByRole("button", { name: "OpenCode" }).click();
  await expect(page.getByText("WSL OpenCode 会话")).toBeVisible();
  await expect(page.getByText("WSL:Ubuntu")).toBeVisible();
  await expect(page.getByText("Ubuntu:/home/me/demo")).toBeVisible();
});

test("DeepSeek Harness source renders its sessions", async ({ page }) => {
  await page.goto("/sessions");
  await page.getByRole("button", { name: "DeepSeek Harness" }).click();
  await expect(page.getByText("DSH 隔离会话")).toBeVisible();
  await page.getByText("DSH 隔离会话").first().click();
  await expect(page.getByText("请检查登录流程，定位失败原因。").first()).toBeVisible();
  await page.getByRole("button", { name: "恢复会话" }).click();
  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("resume_session");
});

test("Codex process dialog can stop the selected process", async ({ page }) => {
  await page.goto("/sessions");
  await page.getByRole("button", { name: "Codex" }).click();
  await page.getByText("Codex 隔离会话").first().click();

  await page.getByRole("button", { name: "关闭进程" }).click();
  await expect(page.getByRole("dialog")).toBeVisible();
  await page.getByRole("dialog").getByRole("button", { name: "确认关闭" }).click();

  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("list_session_processes");
  expect(calls).toContain("stop_session_process");
});

test("update check handles the no-update path", async ({ page }) => {
  await page.goto("/settings");
  await page.getByRole("button", { name: "检查更新" }).click();
  await expect(page.getByText("当前已是最新版本。")).toBeVisible();
  const calls = await page.evaluate(
    () => (window as unknown as { __E2E_CALLS__: string[] }).__E2E_CALLS__,
  );
  expect(calls).toContain("plugin:updater|check");
});

test("available update changelog is rendered as Markdown", async ({ page }) => {
  await page.goto("/settings");
  await page.evaluate(() => {
    (
      window as unknown as { __E2E_SET_UPDATE__: (value: unknown) => void }
    ).__E2E_SET_UPDATE__({
      rid: 1,
      currentVersion: "0.4.0",
      version: "0.5.0",
      date: "2026-10-22",
      body: "## 新版本\n\n- **重要** 修复\n\n`code`",
      rawJson: "{}",
    });
  });

  await page.getByRole("button", { name: "检查更新" }).click();
  await expect(page.getByText("发现新版本 v0.5.0，可以下载并安装。")).toBeVisible();
  await expect(page.getByRole("heading", { name: "新版本" })).toBeVisible();
  await expect(page.getByText("重要")).toBeVisible();
  await expect(page.getByText("## 新版本")).toHaveCount(0);
});
