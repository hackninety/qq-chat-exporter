/**
 * UI smoke test - boots the auth page, drops a token and makes sure we land
 * on the main app shell. Skipped automatically when the frontend isn't
 * reachable so this can run in environments where only the API is up.
 *
 * To run these locally:
 *   1. `cd qce-v4-tool && pnpm build`
 *   2. `mkdir -p ../static && rm -rf ../static/qce && cp -r out ../static/qce`
 *   3. `cd ../plugins/qq-chat-exporter && pnpm mock:server`
 *   4. `cd ../../qce-v4-tool && E2E_FRONTEND_URL=http://localhost:40653 pnpm exec playwright test e2e/ui-smoke.spec.ts`
 *
 * The mock server serves the built frontend under `/qce/...` plus the
 * REST API on the same origin, which matches production routing far more
 * closely than `next dev` does.
 */

import { test, expect } from '@playwright/test';
import { isNewerVersion } from '../lib/version';

const TOKEN = process.env.QCE_MOCK_TOKEN ?? 'qce_mock_token_for_tests';
const FRONTEND_BASE = process.env.E2E_FRONTEND_URL ?? 'http://localhost:40653';
// Production URL has the frontend living under `/qce/`.
const AUTH_PATH = `/qce/auth`;
const SHELL_PATH = `/qce`;

async function clearLocalStorage(page: import('@playwright/test').Page) {
    // We can't use addInitScript here – that runs on EVERY navigation in the
    // page, so it would also wipe a token the auth flow just persisted.
    // Use the auth page as a stable same-origin landing page. Visiting the app
    // shell without a token starts an asynchronous redirect that can destroy
    // the execution context while the storage write is running.
    await page.goto(`${FRONTEND_BASE}${AUTH_PATH}`).catch(() => null);
    await page.evaluate(() => localStorage.clear()).catch(() => null);
}

async function authenticate(page: import('@playwright/test').Page) {
    await clearLocalStorage(page);
    await page.goto(`${FRONTEND_BASE}${AUTH_PATH}?token=${TOKEN}`);
    await page.waitForURL(
        (url) => !url.pathname.endsWith('/auth') && !url.pathname.endsWith('/auth/'),
        { timeout: 15_000 }
    );
    await page.evaluate(() => {
        localStorage.setItem('qce-onboarding-completed', 'true');
    });
}

test.describe('Auth flow', () => {

    test('home page loads', async ({ page }) => {
        const response = await page.goto(`${FRONTEND_BASE}${SHELL_PATH}`).catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );
        const title = await page.title();
        expect(title.length).toBeGreaterThan(0);
    });

    /**
     * Issue #287: the server prints a one-click login URL like
     * `…/qce/auth?token=<accessToken>`. The auth page should detect
     * that token, strip it from the URL bar (so history doesn't keep a copy),
     * verify it against the API, persist it to localStorage and forward the
     * user out of `/auth`.
     */
    test('one-click ?token=... strips the query string and persists the token', async ({ page }) => {
        await clearLocalStorage(page);
        const response = await page
            .goto(`${FRONTEND_BASE}${AUTH_PATH}?token=${TOKEN}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // `replaceState` should clear the `?token=` param before verification
        // resolves, so it never hangs around in browser history.
        await page.waitForFunction(() => !window.location.search.includes('token='), {
            timeout: 15_000
        });

        // Wait for the auth redirect to settle before reading storage so navigation does not replace the page context.
        await page.waitForURL(
            (url) => !url.pathname.endsWith('/auth') && !url.pathname.endsWith('/auth/'),
            { timeout: 15_000 }
        );

        const stored = await page.evaluate(() => localStorage.getItem('qce_access_token'));
        expect(stored).toBe(TOKEN);
    });

    /**
     * Bad URL token: mock API rejects, we fall back to the manual form so the
     * user can paste a fresh token from `security.json`. We must NOT redirect
     * and must NOT keep the bogus token in localStorage.
     */
    test('one-click flow falls back to manual form when token is rejected', async ({ page }) => {
        await clearLocalStorage(page);
        const response = await page
            .goto(`${FRONTEND_BASE}${AUTH_PATH}?token=definitely-wrong-token`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // Query string still gets stripped immediately on mount.
        await page.waitForFunction(() => !window.location.search.includes('token='), {
            timeout: 15_000
        });

        // Wait for the verification round-trip to finish; the page should
        // settle on the manual form without a redirect.
        await page.waitForTimeout(1500);
        const stored = await page.evaluate(() => localStorage.getItem('qce_access_token'));
        expect(stored).toBeNull();
        expect(new URL(page.url()).pathname).toMatch(/\/auth\/?$/);
    });

    /**
     * Codex P2 on PR #401: even when the user is already authenticated, the
     * auth page must scrub `?token=` from the URL before redirecting.
     * Otherwise the token-bearing URL stays in history / address bar
     * navigation when the user hits "back".
     */
    test('already-authenticated visit still strips ?token= from history', async ({ page }) => {
        // Pre-seed a valid token so the auth page hits the
        // `authManager.isAuthenticated()` branch.
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        const response = await page
            .goto(`${FRONTEND_BASE}${AUTH_PATH}?token=${TOKEN}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // The auth page's effect should run `replaceState` to strip the
        // ?token= query before kicking off the redirect.
        await page.waitForFunction(() => !window.location.search.includes('token='), {
            timeout: 15_000
        });

        // The URL the browser would record in history (after replaceState)
        // must no longer contain the token.
        expect(page.url()).not.toContain('token=');
    });
});

test.describe('Version updates', () => {
    test('compares prerelease and stable versions in release order', () => {
        expect(isNewerVersion('v6.0.0-beta.65', '6.0.0-beta.64')).toBe(true);
        expect(isNewerVersion('v6.0.0', '6.0.0-rc.2')).toBe(true);
        expect(isNewerVersion('v6.0.0-beta.66', '6.0.0')).toBe(false);
        expect(isNewerVersion('v6.0.1-beta.1', '6.0.0')).toBe(true);
        expect(isNewerVersion('latest', '6.0.0')).toBe(false);
    });

    test('shows a red help indicator and update entry for a newer release', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);
        await page.route(
            'https://api.github.com/repos/shuakami/qq-chat-exporter/releases?**',
            async (route) => {
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify([{
                        tag_name: 'v6.0.0',
                        html_url: 'https://github.com/shuakami/qq-chat-exporter/releases/tag/v6.0.0',
                    }]),
                });
            }
        );

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        const helpButton = page.getByRole('button', { name: '帮助，有新版本 v6.0.0' });
        await expect(helpButton).toBeVisible({ timeout: 15_000 });
        const skipBtn = page.getByRole('button', { name: '跳过' }).first();
        if (await skipBtn.isVisible({ timeout: 1500 }).catch(() => false)) {
            await skipBtn.click();
        }
        await helpButton.click();
        await expect(page.getByText('发现新版本', { exact: true })).toBeVisible();
        await expect(page.getByText('v6.0.0', { exact: true })).toBeVisible();
        await expect(page.getByText('查看更新内容', { exact: true })).toBeVisible();
    });
});

/**
 * Issue #204: 在搜索框里直接输入一个 4-12 位的 QQ 号，如果好友 / 群 / 最近联系人
 * 都搜不到，会话列表的空态会渲染「按 QQ 号反查」卡片，调用
 * `/api/users/lookup?uin=...`。Mock 服务器特地放了一条 uin=77777 的「已注销好友」
 * 会话，让这条链路完整跑起来。
 */
/**
 * 把主页带到「会话」标签页，并等到会话搜索框出现。
 *
 * 主页默认会打开 onboarding 弹窗（"欢迎使用…"）和 overview tab，会盖住测试要点
 * 的搜索框。这里统一处理：先把欢迎弹窗扫掉，再点侧栏的「会话」按钮，最后等真正
 * 的会话搜索 input 出现。
 */
async function openSessionsTab(page: import('@playwright/test').Page) {
    // 欢迎弹窗里的「跳过」按钮可见就关掉；不在则跳过。
    const skipBtn = page.getByRole('button', { name: '跳过' }).first();
    if (await skipBtn.isVisible({ timeout: 1500 }).catch(() => false)) {
        await skipBtn.click().catch(() => null);
    }
    // 侧栏的「会话」按钮（id=sessions）。用 role+name 精准定位避免和概览里的
    // 「浏览会话」按钮混淆。
    const sessionsTab = page.getByRole('button', { name: '会话', exact: true });
    await expect(sessionsTab).toBeVisible({ timeout: 15_000 });
    await sessionsTab.click();

    const searchBox = page.locator('input[placeholder*="搜索会话"]').first();
    await expect(searchBox).toBeVisible({ timeout: 15_000 });
    return searchBox;
}

test.describe('Session list — QQ lookup (issue #204)', () => {
    test('searching by deactivated QQ number reveals the lookup card', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        const searchBox = await openSessionsTab(page);

        // 输入一个 fixture 里没有任何文字 / id 命中、但 mock 后端能反查到的 uin。
        await searchBox.fill('77777');

        // 空态里应该出现 lookup 卡片标题。
        await expect(page.getByText('按 QQ 号反查会话')).toBeVisible({ timeout: 10_000 });

        // 卡片里有自己的输入框（已经被 initialUin 填上），点查询按钮触发后端调用。
        await page.getByRole('button', { name: /查询/ }).click();

        // 反查到 u_deactivated_77777，按钮区出现「导出」、徽章里写「非好友 / 已注销」。
        await expect(page.getByText('非好友 / 已注销')).toBeVisible({ timeout: 10_000 });
    });

    /**
     * Issue #363: 当本次导出有资源下载失败时，资源统计和 Rkey 降级说明
     * 都收进消息数旁的帮助 tooltip，不额外占用任务卡片空间。
     */
    test('completed task with failed resources shows summary details only in the tooltip (issue #363)', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        // 拦截 /api/tasks，让前端看到一条 issue #363 场景的完成任务。
        await page.route('**/api/tasks', async (route, request) => {
            // 只拦 GET，POST 走真接口（不影响其它流程）。
            if (request.method() !== 'GET') {
                await route.continue();
                return;
            }
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({
                    success: true,
                    data: {
                        tasks: [
                            {
                                id: 'rkey-fallback-task',
                                peer: { peerUid: '12345', chatType: 1 },
                                sessionName: 'Rkey 降级测试会话',
                                status: 'completed',
                                progress: 100,
                                format: 'HTML',
                                messageCount: 200,
                                fileName: 'rkey_test.html',
                                filePath: '/tmp/rkey_test.html',
                                fileSize: 12345,
                                createdAt: new Date(Date.now() - 3 * 60 * 1000).toISOString(),
                                completedAt: new Date(Date.now() - 1 * 60 * 1000).toISOString(),
                                resourceSummary: {
                                    attempted: 12,
                                    alreadyAvailable: 3,
                                    downloaded: 5,
                                    failed: 4,
                                    skipped: 0,
                                    failedSamples: ['photo-1.jpg', 'photo-2.jpg', 'photo-3.jpg', 'photo-4.jpg'],
                                },
                            },
                        ],
                    },
                }),
            });
        });

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // 关掉欢迎弹窗（如有），切到任务标签页。
        const skipBtn = page.getByRole('button', { name: '跳过' }).first();
        if (await skipBtn.isVisible({ timeout: 1500 }).catch(() => false)) {
            await skipBtn.click().catch(() => null);
        }
        const tasksTab = page.getByRole('button', { name: '任务', exact: true });
        await expect(tasksTab).toBeVisible({ timeout: 15_000 });
        await tasksTab.click();

        // 任务行出现
        await expect(page.getByText('Rkey 降级测试会话')).toBeVisible({ timeout: 10_000 });
        await expect(page.getByText(/资源 8\/12，失败 4/)).toHaveCount(0);
        await expect(page.getByText(/Rkey 服务临时降级|重新打开相关消息/)).toHaveCount(0);
        const resourceHelp = page.getByRole('button', { name: '查看资源下载统计' });
        await expect(resourceHelp).toBeVisible();
        await resourceHelp.hover();
        const tooltip = page.getByRole('tooltip');
        await expect(tooltip).toContainText('资源 8/12，失败 4');
        await expect(tooltip).toContainText('QQ Rkey 服务临时不可用');
        const textWrap = await tooltip.evaluate((element) => getComputedStyle(element).textWrap);
        expect(textWrap).not.toContain('balance');
    });

    test('searching for a non-existent QQ shows a friendly not-found message', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        const searchBox = await openSessionsTab(page);
        await searchBox.fill('88888888');

        await expect(page.getByText('按 QQ 号反查会话')).toBeVisible({ timeout: 10_000 });
        await page.getByRole('button', { name: /查询/ }).click();

        // mock 的 getUidByUinV2 对未登记 uin 返 undefined，落到 found=false。
        await expect(page.getByText(/未在本机 NTQQ 数据中找到/)).toBeVisible({ timeout: 10_000 });
    });
});

test.describe('QQ quick login', () => {
    test('plugin mode can clear the locally remembered account', async ({ page }) => {
        await authenticate(page);

        let logoutMethod = '';
        await page.route('**/api/system/logout', async (route, request) => {
            logoutMethod = request.method();
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({
                    success: true,
                    data: {
                        removed: true,
                        currentSessionActive: true,
                        requiresRestart: true,
                        message: '已清除本地自动登录记录；关闭当前程序后，下次启动将显示二维码。',
                    },
                }),
            });
        });

        const response = await page.goto(`${FRONTEND_BASE}/qce/inactive/`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);

        const logoutButton = page.getByTestId('logout-qq-account-button');
        await expect(logoutButton).toBeVisible({ timeout: 15_000 });
        page.once('dialog', (dialog) => dialog.accept());
        await logoutButton.click();

        await expect.poll(() => logoutMethod).toBe('POST');
        await expect(page.getByText('已注销自动登录', { exact: true })).toBeVisible();
    });
});

test.describe('Inactive sessions', () => {
    test('sidebar list previews non-friends and unavailable groups, then opens export preset', async ({ page }) => {
        await authenticate(page);
        const response = await page.goto(`${FRONTEND_BASE}${SHELL_PATH}`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);

        const inactiveTab = page.getByRole('button', { name: '已删除/退出', exact: true });
        await expect(inactiveTab).toBeVisible({ timeout: 15_000 });
        await inactiveTab.click();

        await expect(page.getByRole('button', { name: '全部 (2)' })).toBeVisible({ timeout: 15_000 });
        await expect(page.getByText('已删除的测试好友（66666）', { exact: true })).toBeVisible();
        await expect(page.getByText('u_inactive_66666', { exact: true })).toHaveCount(0);
        await expect(page.getByText('已经退出的测试群（888000）', { exact: true })).toBeVisible();
        await expect(page.getByText('QCE Testing Group', { exact: true })).toHaveCount(0);
        await expect(page.getByText('Alice (Real Name)', { exact: true })).toHaveCount(0);

        await page.getByRole('button', { name: '预览 已删除的测试好友（66666） 聊天记录' }).click();
        await expect(page.getByText('这条消息来自非好友会话', { exact: true })).toBeVisible({ timeout: 10_000 });
        await page.keyboard.press('Escape');
        await expect(page.getByText('这条消息来自非好友会话', { exact: true })).toHaveCount(0);

        await page.getByRole('button', { name: '预览 已经退出的测试群（888000） 聊天记录' }).click();
        await expect(page.getByText('旧群里的最后一条消息', { exact: true })).toBeVisible({ timeout: 10_000 });
        await page.keyboard.press('Escape');

        await page.getByRole('button', { name: '导出 已经退出的测试群 聊天记录' }).click();
        await expect(page.getByRole('heading', { name: '创建导出任务', level: 1 })).toBeVisible({ timeout: 10_000 });
        await expect(page.locator('#sessionName')).toHaveValue('已经退出的测试群');
    });

    test('direct inactive route supports filtering and searching', async ({ page }) => {
        await authenticate(page);
        const response = await page.goto(`${FRONTEND_BASE}/qce/inactive/`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);

        const search = page.getByPlaceholder('搜索名称、QQ号或群号...');
        await expect(search).toBeVisible({ timeout: 15_000 });
        await search.fill('66666');
        await expect(page.getByText('已删除的测试好友（66666）', { exact: true })).toBeVisible();
        await expect(page.getByText('已经退出的测试群（888000）', { exact: true })).toHaveCount(0);

        await search.fill('');
        await page.getByRole('button', { name: '全部 (2)' }).click();
        await page.getByRole('menuitem', { name: /已退出 \/ 不可用群 \(1\)/ }).click();
        await expect(page.getByText('已经退出的测试群（888000）', { exact: true })).toBeVisible();
        await expect(page.getByText('已删除的测试好友（66666）', { exact: true })).toHaveCount(0);
    });

    test('lists imported discussion groups as viewable history sessions', async ({ page }) => {
        await authenticate(page);
        await page.route('**/api/inactive-sessions**', async (route) => {
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({
                    success: true,
                    data: {
                        sessions: [{
                            kind: 'discussion',
                            chatType: 3,
                            peerUid: '99887',
                            peerUin: '99887',
                            name: '讨论组 99887',
                            backupImportId: 'backup-discussion',
                            sourceName: 'nt_msg.db',
                            messageCount: 83657,
                        }],
                        totalCount: 1,
                        nonFriendCount: 0,
                        unavailableGroupCount: 0,
                        discussionCount: 1,
                        rawCount: 0,
                        databaseRawCount: 1,
                        indexSource: 'full',
                        source: 'database',
                    },
                }),
            });
        });

        const response = await page.goto(`${FRONTEND_BASE}/qce/inactive/`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);
        await expect(page.getByRole('button', { name: '预览 讨论组 99887 聊天记录' })).toBeVisible();
        await page.getByRole('button', { name: '全部 (1)' }).click();
        await expect(page.getByRole('menuitem', { name: '讨论组 (1)' })).toBeVisible();
    });

    test('uploads a backup, previews its history and exports with the import id', async ({ page }) => {
        await authenticate(page);
        let imported = false;
        let detectBody: string | null = null;
        let uploadBody: string | null = null;
        let previewBody: Record<string, any> | null = null;
        let exportBody: Record<string, any> | null = null;
        let inactiveRefreshes = 0;
        const backup = {
            id: 'backupfixture01',
            fileName: 'nt_msg_export.db',
            format: 'nt_msg_export',
            createdAt: '2026-08-05T00:00:00Z',
            fileSize: 4096,
            sessionCount: 2,
            messageCount: 3,
        };
        const sessions = [
            {
                importId: backup.id,
                sourceName: backup.fileName,
                format: backup.format,
                chatType: 1,
                peerUid: 'u_backup_deleted',
                peerUin: '45678',
                name: '备份中的已删除好友',
                avatarUrl: '',
                lastMsgTime: '2026-08-04T12:00:00Z',
                messageCount: 2,
            },
            {
                importId: backup.id,
                sourceName: backup.fileName,
                format: backup.format,
                chatType: 2,
                peerUid: '87654',
                peerUin: '87654',
                name: '备份中的已退出群',
                avatarUrl: '',
                lastMsgTime: '2026-08-04T13:00:00Z',
                messageCount: 1,
            },
        ];
        const inactiveSessions = sessions.map((session) => ({
            kind: session.chatType === 2 ? 'unavailable_group' : 'non_friend',
            chatType: session.chatType,
            peerUid: session.peerUid,
            peerUin: session.peerUin,
            name: session.name,
            avatarUrl: session.avatarUrl,
            lastMsgTime: session.lastMsgTime,
            messageCount: session.messageCount,
            backupImportId: session.importId,
            sourceName: session.sourceName,
        }));

        await page.route('**/api/inactive-sessions**', async (route) => {
            inactiveRefreshes += 1;
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({
                    success: true,
                    data: {
                        sessions: imported ? inactiveSessions : [],
                        totalCount: imported ? 2 : 0,
                        nonFriendCount: imported ? 1 : 0,
                        unavailableGroupCount: imported ? 1 : 0,
                        rawCount: 0,
                        databaseRawCount: imported ? 2 : 0,
                        indexSource: 'full',
                        source: imported ? 'database' : 'full',
                    },
                }),
            });
        });
        await page.route('**/api/chat-backups**', async (route, request) => {
            const url = new URL(request.url());
            if (request.method() === 'POST' && url.pathname.endsWith('/detect-key')) {
                detectBody = request.postData();
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({
                        success: true,
                        data: {
                            required: true,
                            detected: true,
                            key: 'auto-detected-key',
                            source: 'qq_memory',
                        },
                    }),
                });
                return;
            }
            if (request.method() === 'POST' && url.pathname.endsWith('/upload')) {
                uploadBody = request.postData();
                imported = true;
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({ success: true, data: { import: backup } }),
                });
                return;
            }
            if (request.method() === 'GET' && url.pathname.endsWith('/sessions')) {
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({ success: true, data: { sessions: imported ? sessions : [] } }),
                });
                return;
            }
            if (request.method() === 'GET' && url.pathname.endsWith('/api/chat-backups')) {
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({ success: true, data: { imports: imported ? [backup] : [] } }),
                });
                return;
            }
            await route.continue();
        });
        await page.route('**/api/messages/fetch', async (route, request) => {
            previewBody = request.postDataJSON();
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({
                    success: true,
                    data: {
                        messages: [{
                            msgId: '1', msgSeq: '1', msgTime: '1722772800', chatType: 1,
                            senderUid: 'u_sender', senderUin: '10001', peerUid: 'u_backup_deleted',
                            peerUin: '45678', sendType: 0, msgType: 2, subMsgType: 1,
                            sendNickName: '旧联系人', sendMemberName: '',
                            elements: [
                                { elementType: 1, textElement: { content: '来自备份数据库的历史消息' } },
                                {
                                    elementType: 16,
                                    multiForwardMsgElement: {
                                        resId: '',
                                        xmlContent: JSON.stringify({
                                            app: 'com.tencent.gamecenter.mall',
                                            prompt: '活动卡片',
                                            url: 'https://example.com/card',
                                        }),
                                    },
                                },
                            ],
                        }],
                        totalCount: 1, currentPage: 1, totalPages: 1, hasNext: false,
                    },
                }),
            });
        });
        await page.route('**/api/messages/export', async (route, request) => {
            exportBody = request.postDataJSON();
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({ success: true, data: { taskId: 'backup-export-task' } }),
            });
        });

        const response = await page.goto(`${FRONTEND_BASE}/qce/inactive/`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);

        await expect(page.getByText('导入的聊天记录备份')).toBeVisible({ timeout: 15_000 });
        await page.getByRole('button', { name: '导入备份' }).click();
        await page.locator('input[type="file"]').setInputFiles({
            name: 'nt_msg.db',
            mimeType: 'application/octet-stream',
            buffer: Buffer.from('encrypted-ntqq-fixture'),
        });
        const keyInput = page.getByPlaceholder('NTQQ 数据库密钥（明文导出库可留空）');
        await expect(keyInput).toHaveValue('auto-detected-key');
        await expect(page.getByText('已从本机登录中的 QQ 自动检测并填入数据库密钥。')).toBeVisible();
        await keyInput.fill('manual-override-key');
        await expect(keyInput).toHaveValue('manual-override-key');
        await page.getByRole('button', { name: '自动检测' }).click();
        await expect(keyInput).toHaveValue('auto-detected-key');
        await page.getByRole('button', { name: '开始导入' }).click();

        await expect.poll(() => detectBody).toContain('nt_msg.db');
        await expect.poll(() => detectBody).toContain('originalSize');
        await expect.poll(() => uploadBody).toContain('auto-detected-key');
        await expect.poll(() => uploadBody).toContain('nt_msg.db');
        await expect.poll(() => inactiveRefreshes).toBeGreaterThanOrEqual(2);
        await expect(page.getByText('已合并导入数据库中的 2 个历史会话，并排除仍在当前好友或群列表中的对象。')).toBeVisible();
        await expect(page.getByText('备份中的已删除好友（45678）', { exact: true })).toHaveCount(1);
        await expect(page.getByText('u_backup_deleted', { exact: true })).toHaveCount(0);
        await expect(page.getByText('备份中的已退出群（87654）', { exact: true })).toHaveCount(1);

        const sessionButtons = page.getByRole('button', { name: /^预览 备份中的/ });
        await expect(sessionButtons).toHaveCount(2);
        await expect(sessionButtons.nth(0)).toHaveAttribute('aria-label', '预览 备份中的已退出群（87654） 聊天记录');

        await page.getByRole('button', { name: '按最后消息时间' }).click();
        await page.getByRole('menuitem', { name: '按聊天记录条数' }).click();
        await expect(sessionButtons.nth(0)).toHaveAttribute('aria-label', '预览 备份中的已删除好友（45678） 聊天记录');

        const backupSearch = page.getByPlaceholder('搜索名称、QQ号或群号...');
        await backupSearch.fill('45678');
        await expect(page.getByText('备份中的已删除好友（45678）', { exact: true })).toBeVisible();
        await expect(page.getByText('备份中的已退出群（87654）', { exact: true })).toHaveCount(0);
        await backupSearch.fill('');

        const privateSession = page.getByRole('button', { name: '预览 备份中的已删除好友（45678） 聊天记录' });
        await privateSession.click();
        await expect(page.getByText('来自备份数据库的历史消息', { exact: true })).toBeVisible();
        await expect(page.getByText(/^2024 \d{2}-\d{2} \d{2}:\d{2}$/)).toBeVisible();
        await expect(page.getByRole('link', { name: '活动卡片' })).toHaveAttribute('href', 'https://example.com/card');
        await expect(page.getByText('[合并转发]', { exact: true })).toHaveCount(0);
        await expect.poll(() => previewBody?.peer).toEqual({
            chatType: 1,
            peerUid: 'u_backup_deleted',
            backupImportId: backup.id,
        });
        await page.keyboard.press('Escape');

        await page.getByRole('button', { name: '导出 备份中的已退出群 聊天记录' }).click();
        await expect(page.getByRole('heading', { name: '创建导出任务', level: 1 })).toBeVisible();
        await expect(page.locator('#sessionName')).toHaveValue('备份中的已退出群');
        await page.getByRole('button', { name: 'QCE Archive', exact: true }).click();
        await expect(page.getByRole('dialog').getByText(/生成可供离线工具读取的 \.qcearchive/)).toBeVisible();
        await page.getByRole('button', { name: '创建任务', exact: true }).click();
        await expect.poll(() => exportBody?.peer).toEqual({
            chatType: 2,
            peerUid: '87654',
            backupImportId: backup.id,
            peerUin: '87654',
            guildId: '',
        });
        await expect.poll(() => exportBody?.format).toBe('QCEARCHIVE');
    });
});

test.describe('Account archive exports', () => {
    test('starts a debug-enabled full account export from the inactive backup section', async ({ page }) => {
        await authenticate(page);

        const backup = {
            id: 'account-backup-1',
            fileName: 'nt_msg.sqlite',
            format: 'nt_msg_export',
            createdAt: '2026-08-05T12:00:00Z',
            fileSize: 1024,
            sessionCount: 88,
            messageCount: 12345,
        };
        let previewBody: { backupImportId?: string } | undefined;
        let createBody: { backupImportId?: string; debugExport?: boolean; resume?: boolean } | undefined;
        await page.route('**/api/chat-backups', async (route, request) => {
            if (request.method() !== 'GET') {
                await route.continue();
                return;
            }
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({ success: true, data: { imports: [backup] } }),
            });
        });
        await page.route('**/api/account-exports/preview', async (route, request) => {
            previewBody = request.postDataJSON();
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({
                    success: true,
                    data: {
                        account: { uid: 'u_self', uin: '123456789', name: '测试账号' },
                        backup,
                        counts: { total: 100, friend: 60, nonFriend: 20, group: 15, unavailableGroup: 4, other: 1 },
                        historicalSessionCount: 88,
                        currentFriendCount: 60,
                        currentGroupCount: 15,
                        localSessionCount: 80,
                        warningCount: 0,
                        warnings: [],
                        resumeAvailable: true,
                        completedCheckpointCount: 37,
                        fixedIncludes: ['messages.sqlite（规范化消息与 FTS5 索引）', 'source/nt_msg.sqlite（已解密源库副本）'],
                        notice: '所选已解密备份将关联到当前登录账号。',
                    },
                }),
            });
        });
        await page.route('**/api/account-exports', async (route, request) => {
            createBody = request.postDataJSON();
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify({ success: true, data: { taskId: 'account-export-test' } }),
            });
        });

        const response = await page.goto(`${FRONTEND_BASE}/qce/inactive/`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);
        await expect(page.getByText('导入的聊天记录备份')).toBeVisible({ timeout: 15_000 });
        await page.getByTestId('inactive-account-export-button').click();

        const dialog = page.getByRole('dialog');
        await expect(dialog.getByRole('heading', { name: '创建导出任务' })).toBeVisible();
        await expect(dialog.locator('select')).toHaveValue(backup.id);
        await expect(dialog.getByText(/历史主源：nt_msg\.sqlite/)).toBeVisible();
        await expect.poll(() => previewBody).toEqual({ backupImportId: backup.id });
        await expect(page.getByText('已删除/非好友', { exact: true })).toBeVisible();
        await expect(page.getByText('已退出/不可用群', { exact: true })).toBeVisible();
        await expect(dialog.getByRole('checkbox', { name: /继续上次未完成的导出/ })).toBeChecked();
        await expect(dialog.getByText(/已保存 37 个会话断点/)).toBeVisible();
        await expect(dialog.getByRole('checkbox', { name: /同时生成 \.debug/ })).toBeChecked();

        await dialog.getByRole('button', { name: '创建导出任务', exact: true }).click();
        await expect.poll(() => createBody).toEqual({ backupImportId: backup.id, debugExport: true, resume: true });
    });
});

test.describe('Sticker exports', () => {
    test('exporting keeps the loaded sticker list visible', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        let releaseExport!: () => void;
        const exportGate = new Promise<void>((resolve) => {
            releaseExport = resolve;
        });
        let markExportStarted!: () => void;
        const exportStarted = new Promise<void>((resolve) => {
            markExportStarted = resolve;
        });

        await page.route('**/api/sticker-packs**', async (route, request) => {
            const url = new URL(request.url());
            if (request.method() === 'GET' && url.pathname.endsWith('/export-records')) {
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({
                        success: true,
                        data: { records: [], totalCount: 0 },
                    }),
                });
                return;
            }
            if (request.method() === 'GET') {
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({
                        success: true,
                        data: {
                            packs: [{
                                packId: 'regression-pack',
                                packName: '回归测试表情包',
                                packType: 'favorite_emoji',
                                stickerCount: 1,
                                stickers: [],
                            }],
                            stats: {
                                favorite_emoji: 1,
                                market_pack: 0,
                                system_pack: 0,
                            },
                            totalCount: 1,
                            totalStickers: 1,
                        },
                    }),
                });
                return;
            }
            if (request.method() === 'POST' && url.pathname.endsWith('/export')) {
                markExportStarted();
                await exportGate;
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({
                        success: true,
                        data: {
                            success: true,
                            packCount: 1,
                            stickerCount: 1,
                            exportPath: '/tmp/sticker-export',
                        },
                    }),
                });
                return;
            }
            await route.continue();
        });

        const response = await page
            .goto(`${FRONTEND_BASE}/qce/stickers`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        const packName = page.getByText('回归测试表情包', { exact: true });
        await expect(packName).toBeVisible({ timeout: 15_000 });
        const skipBtn = page.getByRole('button', { name: '跳过' }).first();
        if (await skipBtn.isVisible({ timeout: 1500 }).catch(() => false)) {
            await skipBtn.click();
        }
        const packRow = packName.locator('xpath=ancestor::div[contains(@class,"group")]');
        await packRow.hover();
        await packRow.getByRole('button', { name: '导出', exact: true }).click();
        await exportStarted;

        await expect(packName).toBeVisible();
        await expect(page.getByText('正在加载表情包...')).toBeHidden();

        releaseExport();
        await expect(page.getByText('表情包“回归测试表情包”已导出')).toBeVisible({
            timeout: 15_000,
        });
    });
});

/**
 * Issue #346: 网络抽风 / 中间代理篡改时，POST /auth 可能短暂返回 5xx 或被
 * 改写成 success=false。老 AuthProvider 只要 success 不为 truthy 就清掉本地
 * token + 跳回 /auth，把已经登录的用户踢出。新版只有 401 / 403 才会清 token，
 * 其它一律放行。这里通过 page.route 模拟两种场景：
 *   1. POST /auth 返回 502 → 用户保留 token，停留在主界面
 *   2. POST /auth 返回 200 + `{ success: false }` 但状态码不是 401/403 → 同上
 */
test.describe('Auth validation resilience (issue #346)', () => {
    test('transient 502 on /auth keeps the user inside the app', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        // 用 page.route 拦 POST /auth，在第一次校验请求上返回 502；后续别的
        // /auth 流程都走真接口。
        let blocked = false;
        await page.route('**/auth', async (route, request) => {
            if (!blocked && request.method() === 'POST') {
                blocked = true;
                await route.fulfill({
                    status: 502,
                    contentType: 'text/html',
                    body: '<html><body>Bad Gateway</body></html>',
                });
                return;
            }
            await route.continue();
        });

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // 等到 AuthProvider 走完 / 渲染主界面：侧栏一定有「会话」入口。
        await expect(page.getByRole('button', { name: '会话', exact: true }))
            .toBeVisible({ timeout: 15_000 });

        // 用户没有被踢回 /auth。
        expect(new URL(page.url()).pathname).not.toMatch(/\/auth\/?$/);

        // localStorage 里的 token 也没被清掉。
        const stored = await page.evaluate(() => localStorage.getItem('qce_access_token'));
        expect(stored).toBe(TOKEN);
    });

    test('non-401/403 with success:false body still keeps the user inside the app', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        let blocked = false;
        await page.route('**/auth', async (route, request) => {
            if (!blocked && request.method() === 'POST') {
                blocked = true;
                await route.fulfill({
                    status: 200,
                    contentType: 'application/json',
                    body: JSON.stringify({
                        success: false,
                        error: { type: 'PROXY_TAMPERING', message: 'reverse proxy ate the body' },
                    }),
                });
                return;
            }
            await route.continue();
        });

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        await expect(page.getByRole('button', { name: '会话', exact: true }))
            .toBeVisible({ timeout: 15_000 });
        expect(new URL(page.url()).pathname).not.toMatch(/\/auth\/?$/);
        const stored = await page.evaluate(() => localStorage.getItem('qce_access_token'));
        expect(stored).toBe(TOKEN);
    });

    test('explicit 403 still clears token and redirects to /auth', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        await page.route('**/auth', async (route, request) => {
            if (request.method() === 'POST') {
                await route.fulfill({
                    status: 403,
                    contentType: 'application/json',
                    body: JSON.stringify({
                        success: false,
                        error: { type: 'AUTH_ERROR', message: 'invalid token', context: { code: 'INVALID_TOKEN' } },
                    }),
                });
                return;
            }
            await route.continue();
        });

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // 真 token 失效，前端应当踢回 /auth 并清掉 localStorage。
        await page.waitForURL(/\/auth\/?$/, { timeout: 15_000 });
        const stored = await page.evaluate(() => localStorage.getItem('qce_access_token'));
        expect(stored).toBeNull();
    });
});

/**
 * Issue #340: 独立模式（start-standalone.bat）下没有 NapCat / QQ 登录态。
 * 老版本进入 sessions 标签页会立刻发起 /api/friends + /api/groups，两个端点
 * 都回 503 STANDALONE_MODE，前端在右上角连弹两次红色 toast，且 SessionList
 * 卡在「加载中」。这里通过路由拦截把 /api/system/info 的 mode 改成 'standalone'，
 * 验证前端会换成专门的引导卡片，并不再发出 friends / groups 请求。
 */
test.describe('Standalone mode (issue #340)', () => {
    test('sessions tab shows a standalone banner instead of loading friends/groups', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => {
            localStorage.setItem('qce_access_token', value);
        }, TOKEN);

        // 拦 /api/system/info：把 mode 改成 standalone，napcat.online 改成 false。
        await page.route('**/api/system/info', async (route, request) => {
            if (request.method() !== 'GET') {
                await route.continue();
                return;
            }
            const original = await route.fetch();
            const json = await original.json();
            if (json?.success && json.data) {
                json.data.mode = 'standalone';
                if (json.data.napcat) {
                    json.data.napcat.online = false;
                }
            }
            await route.fulfill({
                status: 200,
                contentType: 'application/json',
                body: JSON.stringify(json),
            });
        });

        // 监控 friends / groups，应当一次都不被请求。
        const friendsRequests: string[] = [];
        const groupsRequests: string[] = [];
        page.on('request', (req) => {
            const url = req.url();
            if (url.includes('/api/friends')) friendsRequests.push(url);
            if (url.includes('/api/groups')) groupsRequests.push(url);
        });

        const response = await page
            .goto(`${FRONTEND_BASE}${SHELL_PATH}`)
            .catch(() => null);
        test.skip(
            !response || response.status() >= 500,
            `frontend not reachable at ${FRONTEND_BASE}`
        );

        // 跳过欢迎弹窗（如有）。
        const skipBtn = page.getByRole('button', { name: '跳过' }).first();
        if (await skipBtn.isVisible({ timeout: 1500 }).catch(() => false)) {
            await skipBtn.click().catch(() => null);
        }

        const sessionsTab = page.getByRole('button', { name: '会话', exact: true });
        await expect(sessionsTab).toBeVisible({ timeout: 15_000 });
        await expect(page.getByRole('button', { name: '已删除/退出', exact: true })).toHaveCount(0);
        await sessionsTab.click();
        await expect(page.getByTestId('logout-qq-account-button')).toHaveCount(0);
        await expect(page.getByTestId('account-export-button')).toHaveCount(0);
        await expect(page.getByTestId('inactive-account-export-button')).toHaveCount(0);

        // 引导卡片可见。
        const banner = page.getByTestId('sessions-standalone-banner');
        await expect(banner).toBeVisible({ timeout: 10_000 });
        await expect(banner.getByText('当前是独立模式')).toBeVisible();
        await expect(banner.getByRole('button', { name: /浏览聊天记录/ })).toBeVisible();

        // 给一点时间确保前端没有偷偷发请求。
        await page.waitForTimeout(800);
        expect(friendsRequests, 'standalone mode must skip /api/friends').toEqual([]);
        expect(groupsRequests, 'standalone mode must skip /api/groups').toEqual([]);

        // 点击「浏览聊天记录」直接跳到 history 标签页。history 标签页的内容区
        // 一定带「记录列表」这个 segment 切换按钮，用它来确认跳转成功。
        await banner.getByRole('button', { name: /浏览聊天记录/ }).click();
        await expect(
            page.getByRole('button', { name: '记录列表', exact: true })
        ).toBeVisible({ timeout: 10_000 });
    });
});

test.describe('Scheduled exports (issue #624)', () => {
    test('edits an existing task and triggers the selected tasks in one batch', async ({ page }) => {
        await clearLocalStorage(page);
        await page.evaluate((value) => localStorage.setItem('qce_access_token', value), TOKEN);

        let tasks = [
            {
                id: 'task-a', name: '任务 A', peer: { chatType: 2, peerUid: 'group-a', guildId: '' },
                sessionName: '群 A', scheduleType: 'daily', executeTime: '02:00',
                timeRangeType: 'yesterday', format: 'JSON', enabled: true,
                options: { filterPureImageMessages: false },
            },
            {
                id: 'task-b', name: '任务 B', peer: { chatType: 1, peerUid: 'friend-b', guildId: '' },
                sessionName: '好友 B', scheduleType: 'weekly', executeTime: '03:00',
                timeRangeType: 'last-week', format: 'JSON', enabled: false, options: {},
            },
        ];
        let updateBody: Record<string, unknown> | null = null;
        let batchBody: { ids?: string[] } | null = null;

        await page.route('**/api/scheduled-exports**', async (route, request) => {
            const url = new URL(request.url());
            if (request.method() === 'GET' && url.pathname.endsWith('/api/scheduled-exports')) {
                await route.fulfill({
                    status: 200, contentType: 'application/json',
                    body: JSON.stringify({ success: true, data: { scheduledExports: tasks } }),
                });
                return;
            }
            if (request.method() === 'PUT' && url.pathname.endsWith('/api/scheduled-exports/task-a')) {
                updateBody = request.postDataJSON();
                tasks = tasks.map(task => task.id === 'task-a' ? { ...task, ...updateBody } : task);
                await route.fulfill({
                    status: 200, contentType: 'application/json',
                    body: JSON.stringify({ success: true, data: { ...tasks[0], id: 'task-a' } }),
                });
                return;
            }
            if (request.method() === 'POST' && url.pathname.endsWith('/api/scheduled-exports/trigger-batch')) {
                batchBody = request.postDataJSON();
                await route.fulfill({
                    status: 200, contentType: 'application/json',
                    body: JSON.stringify({
                        success: true,
                        data: { triggeredCount: batchBody?.ids?.length ?? 0, triggered: [], missingIds: [] },
                    }),
                });
                return;
            }
            await route.continue();
        });

        const response = await page.goto(`${FRONTEND_BASE}${SHELL_PATH}/scheduled`).catch(() => null);
        test.skip(!response || response.status() >= 500, `frontend not reachable at ${FRONTEND_BASE}`);
        const skipBtn = page.getByRole('button', { name: '跳过' }).first();
        if (await skipBtn.isVisible({ timeout: 1500 }).catch(() => false)) await skipBtn.click();

        await expect(page.getByText('任务 A', { exact: true })).toBeVisible({ timeout: 15_000 });
        await page.getByRole('button', { name: '编辑', exact: true }).first().click();
        await expect(page.getByRole('dialog', { name: '编辑定时导出任务' })).toBeVisible();
        await page.locator('#namePrefix').fill('任务 A 已编辑');
        await page.getByRole('button', { name: '保存更改', exact: true }).click();
        await expect.poll(() => updateBody?.name).toBe('任务 A 已编辑');
        await expect.poll(() => (updateBody?.options as Record<string, unknown> | undefined)?.filterPureImageMessages).toBe(false);

        await page.getByRole('button', { name: '批量选择', exact: true }).click();
        await page.getByRole('checkbox', { name: '选择定时任务 任务 A 已编辑' }).click();
        await page.getByRole('checkbox', { name: '选择定时任务 任务 B' }).click();
        await page.getByRole('button', { name: '执行', exact: true }).click();
        await expect.poll(() => batchBody?.ids).toEqual(['task-a', 'task-b']);
    });
});
