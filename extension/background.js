// turbo-dl 浏览器扩展：后台服务（右键菜单发送链接）。
// 需要在弹窗设置中填写 token 和端口（token 见 turbo-dl 启动日志或
// %APPDATA%\turbo-dl\local_token）。

const DEFAULT_PORT = 31877;

async function getSettings() {
  return chrome.storage.local.get({ token: '', port: DEFAULT_PORT });
}

function baseUrl(port) {
  return `http://127.0.0.1:${port}`;
}

// 创建/更新右键菜单（token 未配置时给出提示）。
async function updateMenu() {
  const { token, port } = await getSettings();
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: 'send-to-turbo-dl',
      title: token
        ? '发送到 turbo-dl 下载'
        : '发送到 turbo-dl（未配置 token）',
      contexts: ['link'],
    });
    if (token) {
      chrome.contextMenus.create({
        id: 'open-turbo-dl-popup',
        title: `打开 turbo-dl（:${port}）`,
        contexts: ['all'],
      });
    }
  });
}

chrome.runtime.onInstalled.addListener(updateMenu);
chrome.runtime.onStartup.addListener(updateMenu);
chrome.storage.onChanged.addListener(updateMenu);
chrome.contextMenus.onClicked.addListener(async (info) => {
  if (info.menuItemId === 'send-to-turbo-dl') {
    const { token, port } = await getSettings();
    if (!token || !info.linkUrl) {
      return;
    }
    const url = `${baseUrl(port)}/add?token=${encodeURIComponent(
      token
    )}&url=${encodeURIComponent(info.linkUrl)}`;
    try {
      const resp = await fetch(url);
      const text = await resp.text();
      console.log('[turbo-dl] 发送结果', resp.status, text);
    } catch (e) {
      console.error('[turbo-dl] 发送失败（请确认 turbo-dl 已运行）', e);
    }
  } else if (info.menuItemId === 'open-turbo-dl-popup') {
    const { port } = await getSettings();
    try {
      await fetch(`${baseUrl(port)}/tasks?token=${encodeURIComponent((await getSettings()).token)}`);
    } catch (e) {
      // 忽略：仅用于确认服务在运行
    }
  }
});

// 允许其他页面（popup）通过消息触发发送，便于未来扩展。
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === 'send-url') {
    (async () => {
      const { token, port } = await getSettings();
      if (!token) {
        sendResponse({ ok: false, error: '未配置 token' });
        return;
      }
      const url = `${baseUrl(port)}/add?token=${encodeURIComponent(
        token
      )}&url=${encodeURIComponent(msg.url)}`;
      try {
        const resp = await fetch(url);
        const text = await resp.text();
        sendResponse({ ok: resp.ok, text });
      } catch (e) {
        sendResponse({ ok: false, error: String(e) });
      }
    })();
    return true; // 异步响应
  }
});