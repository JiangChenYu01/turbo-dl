// turbo-dl 弹窗：配置 token/端口、添加链接、查看与控制任务。
const DEFAULT_PORT = 31877;
let settings = { token: '', port: DEFAULT_PORT };

function $(id) {
  return document.getElementById(id);
}

async function loadSettings() {
  settings = await chrome.storage.local.get({ token: '', port: DEFAULT_PORT });
  $('token').value = settings.token;
  $('port').value = settings.port;
}

function baseUrl() {
  return `http://127.0.0.1:${settings.port}`;
}

// 调用本地 API，返回 Response。
async function api(path, params, options) {
  const qs = new URLSearchParams({ token: settings.token, ...params });
  const url = `${baseUrl()}${path}?${qs.toString()}`;
  return fetch(url, options);
}

function statusText(s) {
  return (
    {
      Queued: '排队中',
      Downloading: '下载中',
      Paused: '已暂停',
      Completed: '已完成',
      Failed: '失败',
      Cancelled: '已取消',
    }[s] || s
  );
}

function escapeHtml(s) {
  return String(s).replace(
    /[&<>"']/g,
    (c) =>
      ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])
  );
}

async function refreshTasks() {
  const list = $('tasks');
  let data;
  try {
    const resp = await api('/tasks');
    data = await resp.json();
  } catch (e) {
    list.innerHTML = '<li class="err">无法连接 turbo-dl（请确认程序已运行）</li>';
    return;
  }
  if (!data || !data.tasks) {
    list.innerHTML = '<li class="err">token 错误或无任务</li>';
    return;
  }
  if (data.tasks.length === 0) {
    list.innerHTML = '<li>暂无任务</li>';
    return;
  }
  list.innerHTML = '';
  for (const t of data.tasks) {
    const pct = t.total > 0 ? Math.min(100, Math.round((t.downloaded / t.total) * 100)) : 0;
    const li = document.createElement('li');
    const canControl =
      t.status === 'Downloading' || t.status === 'Queued' || t.status === 'Paused';
    li.innerHTML = `
      <div class="name" title="${escapeHtml(t.url)}">${escapeHtml(t.file_name)}</div>
      <div class="meta">[${escapeHtml(t.protocol)}] ${statusText(t.status)} ${pct}% · ${t.error ? escapeHtml(t.error) : ''}</div>
      <div class="bar"><div class="fill" style="width:${pct}%"></div></div>
      <div class="actions">
        ${
          t.status === 'Paused'
            ? `<button data-act="resume" data-id="${t.id}">继续</button>`
            : t.status === 'Downloading' || t.status === 'Queued'
              ? `<button data-act="pause" data-id="${t.id}">暂停</button><button data-act="cancel" data-id="${t.id}">取消</button>`
              : ''
        }
        <button data-act="remove" data-id="${t.id}">移除</button>
      </div>`;
    void canControl;
    list.appendChild(li);
  }
}

// 任务列表按钮事件（事件委托）
$('tasks').addEventListener('click', async (e) => {
  const btn = e.target.closest('button');
  if (!btn) return;
  const { act, id } = btn.dataset;
  try {
    await api(`/${act}/${id}`, {}, { method: 'POST' });
  } catch (err) {
    $('status').textContent = '操作失败：' + err;
  }
  refreshTasks();
});

// 保存设置
$('save').addEventListener('click', async () => {
  settings.token = $('token').value.trim();
  settings.port = parseInt($('port').value, 10) || DEFAULT_PORT;
  await chrome.storage.local.set(settings);
  $('status').textContent = '设置已保存';
  refreshTasks();
});

// 添加任务
async function addUrl(url) {
  const resp = await api('/add', { url });
  const text = await resp.text();
  $('status').textContent = resp.ok ? text : '添加失败：' + text;
}

$('add').addEventListener('click', () => {
  const url = $('url').value.trim();
  if (!url) return;
  addUrl(url);
  $('url').value = '';
});

$('url').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') $('add').click();
});

// 初始化
loadSettings();
refreshTasks();
setInterval(refreshTasks, 2000);