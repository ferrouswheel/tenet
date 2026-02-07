// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let myPeerId = '';
let currentFilter = '';
let currentView = 'timeline'; // 'timeline', 'conversations', 'conversation-detail', 'post-detail'
let messages = [];
let oldestTimestamp = null;
let ws = null;
let peers = [];
let conversations = [];
let currentConversationPeerId = null;
let conversationMessages = [];
let conversationOldestTimestamp = null;
let currentPostId = null;
const PAGE_SIZE = 50;

// ---------------------------------------------------------------------------
// API helpers
// ---------------------------------------------------------------------------
async function apiGet(path) {
    const res = await fetch(path);
    if (!res.ok) throw new Error(await res.text());
    return res.json();
}
async function apiPost(path, body) {
    const res = await fetch(path, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'request failed' }));
        throw new Error(err.error || 'request failed');
    }
    return res.json();
}
async function apiDelete(path) {
    const res = await fetch(path, { method: 'DELETE' });
    if (!res.ok) throw new Error(await res.text());
    return res.json();
}

// ---------------------------------------------------------------------------
// Toast
// ---------------------------------------------------------------------------
let toastTimer = null;
function showToast(msg) {
    const el = document.getElementById('toast');
    el.textContent = msg;
    el.classList.add('visible');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => el.classList.remove('visible'), 3000);
}

// ---------------------------------------------------------------------------
// Time formatting
// ---------------------------------------------------------------------------
function timeAgo(ts) {
    const diff = Math.floor(Date.now() / 1000) - ts;
    if (diff < 60) return 'just now';
    if (diff < 3600) return Math.floor(diff / 60) + 'm ago';
    if (diff < 86400) return Math.floor(diff / 3600) + 'h ago';
    return new Date(ts * 1000).toLocaleDateString();
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------
function renderMessage(m) {
    const isSelf = m.sender_id === myPeerId;
    const senderLabel = isSelf ? 'You' : m.sender_id.substring(0, 12) + '...';
    const senderClass = isSelf ? 'msg-sender self' : 'msg-sender';
    const unreadClass = m.is_read ? '' : ' unread';
    const kindClass = m.message_kind || 'direct';

    // Make public messages clickable to view detail
    const isPublic = m.message_kind === 'public';
    const clickableClass = isPublic ? ' clickable' : '';
    const onclick = isPublic
        ? `onclick="showPostDetail('${m.message_id}')"`
        : `onclick="markRead('${m.message_id}')"`;

    return `<div class="message${unreadClass}${clickableClass}" data-id="${m.message_id}" ${onclick}>
        <div class="msg-header">
            <span class="msg-badge ${kindClass}">${kindClass}</span>
            <span class="${senderClass}" title="${m.sender_id}">${senderLabel}</span>
            <span class="msg-time">${timeAgo(m.timestamp)}</span>
        </div>
        <div class="msg-body">${escapeHtml(m.body || '')}</div>
    </div>`;
}

function escapeHtml(text) {
    const el = document.createElement('div');
    el.textContent = text;
    return el.innerHTML;
}

function renderTimeline() {
    const el = document.getElementById('timeline');
    const empty = document.getElementById('empty-state');
    const loadMoreEl = document.getElementById('load-more');

    if (messages.length === 0) {
        el.innerHTML = '';
        empty.style.display = '';
        loadMoreEl.style.display = 'none';
        return;
    }

    empty.style.display = 'none';
    el.innerHTML = messages.map(renderMessage).join('');
    loadMoreEl.style.display = messages.length >= PAGE_SIZE ? '' : 'none';
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------
async function loadMessages(append) {
    let url = `/api/messages?limit=${PAGE_SIZE}`;
    if (currentFilter) url += `&kind=${currentFilter}`;
    if (append && oldestTimestamp) url += `&before=${oldestTimestamp}`;

    try {
        const data = await apiGet(url);
        if (append) {
            messages = messages.concat(data);
        } else {
            messages = data;
        }
        if (messages.length > 0) {
            oldestTimestamp = messages[messages.length - 1].timestamp;
        }
        renderTimeline();
    } catch (e) {
        showToast('Failed to load messages');
    }
}

function loadMore() {
    loadMessages(true);
}

// ---------------------------------------------------------------------------
// Filter & View Switching
// ---------------------------------------------------------------------------
document.querySelectorAll('.filters button').forEach(btn => {
    btn.addEventListener('click', () => {
        document.querySelectorAll('.filters button').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');

        if (btn.dataset.view === 'conversations') {
            currentView = 'conversations';
            showConversationsView();
        } else {
            currentView = 'timeline';
            currentFilter = btn.dataset.kind;
            oldestTimestamp = null;
            showTimelineView();
            loadMessages(false);
        }
    });
});

function showTimelineView() {
    document.getElementById('timeline').style.display = '';
    document.getElementById('load-more').style.display = '';
    document.getElementById('empty-state').style.display = 'none';
    document.getElementById('conversations-list').style.display = 'none';
    document.getElementById('conversation-detail').classList.remove('visible');
    document.getElementById('post-detail').classList.remove('visible');
    document.getElementById('compose-kind').value = currentFilter || 'public';
    updateComposeOptions();
}

function showConversationsView() {
    document.getElementById('timeline').style.display = 'none';
    document.getElementById('load-more').style.display = 'none';
    document.getElementById('empty-state').style.display = 'none';
    document.getElementById('conversations-list').style.display = '';
    document.getElementById('conversation-detail').classList.remove('visible');
    document.getElementById('post-detail').classList.remove('visible');
    loadConversations();
}

function showConversationDetail(peerId) {
    currentView = 'conversation-detail';
    currentConversationPeerId = peerId;
    conversationOldestTimestamp = null;
    document.getElementById('timeline').style.display = 'none';
    document.getElementById('load-more').style.display = 'none';
    document.getElementById('conversations-list').style.display = 'none';
    document.getElementById('conversation-detail').classList.add('visible');
    document.getElementById('post-detail').classList.remove('visible');

    const peer = peers.find(p => p.peer_id === peerId);
    const peerName = peer?.display_name || peerId.substring(0, 12) + '...';
    document.getElementById('conversation-peer-name').textContent = peerName;

    document.getElementById('compose-kind').value = 'direct';
    updateComposeOptions();
    loadConversationMessages(peerId, false);
}

function backToConversations() {
    currentView = 'conversations';
    currentConversationPeerId = null;
    showConversationsView();
}

// ---------------------------------------------------------------------------
// Post detail view
// ---------------------------------------------------------------------------
async function showPostDetail(messageId) {
    currentView = 'post-detail';
    currentPostId = messageId;

    document.getElementById('timeline').style.display = 'none';
    document.getElementById('load-more').style.display = 'none';
    document.getElementById('conversations-list').style.display = 'none';
    document.getElementById('conversation-detail').classList.remove('visible');
    document.getElementById('post-detail').classList.add('visible');

    try {
        const post = await apiGet(`/api/messages/${messageId}`);
        renderPostDetail(post);
        // Mark as read when viewing
        if (!post.is_read) {
            await apiPost(`/api/messages/${messageId}/read`, {});
        }
    } catch (e) {
        showToast('Failed to load post');
        console.error(e);
    }
}

function renderPostDetail(post) {
    const el = document.getElementById('post-detail-content');
    const isSelf = post.sender_id === myPeerId;
    const senderLabel = isSelf ? 'You' : post.sender_id.substring(0, 12) + '...';
    const senderClass = isSelf ? 'post-sender self' : 'post-sender';
    const timestamp = new Date(post.timestamp * 1000).toLocaleString();

    el.innerHTML = `
        <div class="${senderClass}" title="${post.sender_id}">${escapeHtml(senderLabel)}</div>
        <div class="post-time">${timestamp}</div>
        <div class="post-body">${escapeHtml(post.body || '')}</div>
    `;
}

function backToPublicFeed() {
    currentView = 'timeline';
    currentPostId = null;
    // Set filter to public to return to public feed
    currentFilter = 'public';

    // Update active filter button
    document.querySelectorAll('.filters button').forEach(b => b.classList.remove('active'));
    document.querySelector('.filters button[data-kind="public"]').classList.add('active');

    showTimelineView();
    loadMessages(false);
}

function updateComposeOptions() {
    const select = document.getElementById('compose-kind');
    select.innerHTML = '';

    if (currentView === 'conversation-detail') {
        // In conversation detail, only show direct message option
        const opt = document.createElement('option');
        opt.value = 'direct';
        opt.textContent = 'Direct to ' + (peers.find(p => p.peer_id === currentConversationPeerId)?.display_name || 'peer');
        select.appendChild(opt);
    } else if (currentFilter === 'friend_group') {
        const opt = document.createElement('option');
        opt.value = 'group';
        opt.textContent = 'Group';
        select.appendChild(opt);
    } else {
        const opt = document.createElement('option');
        opt.value = 'public';
        opt.textContent = 'Public';
        select.appendChild(opt);
    }
}

// ---------------------------------------------------------------------------
// Conversations
// ---------------------------------------------------------------------------
async function loadConversations() {
    try {
        conversations = await apiGet('/api/conversations');
        renderConversationsList();
    } catch (e) {
        showToast('Failed to load conversations');
        console.error(e);
    }
}

function renderConversationsList() {
    const el = document.getElementById('conversations-list');
    if (conversations.length === 0) {
        el.innerHTML = '<div class="empty"><div class="icon">&#128172;</div><div>No conversations yet</div></div>';
        return;
    }

    el.innerHTML = conversations.map(c => {
        const displayName = c.display_name || c.peer_id.substring(0, 12) + '...';
        const preview = c.last_message ? escapeHtml(c.last_message.substring(0, 60)) : 'No messages yet';
        const unreadClass = c.unread_count > 0 ? 'unread' : '';
        const unreadBadge = c.unread_count > 0 ? `<span class="unread-badge">${c.unread_count}</span>` : '';

        return `<div class="conversation-item ${unreadClass}" onclick="showConversationDetail('${c.peer_id}')">
            <div class="conv-header">
                <span class="conv-peer">${escapeHtml(displayName)}</span>
                <div>
                    ${unreadBadge}
                    <span class="conv-time">${timeAgo(c.last_timestamp)}</span>
                </div>
            </div>
            <div class="conv-preview">${preview}</div>
        </div>`;
    }).join('');
}

async function loadConversationMessages(peerId, append) {
    let url = `/api/conversations/${encodeURIComponent(peerId)}?limit=${PAGE_SIZE}`;
    if (append && conversationOldestTimestamp) {
        url += `&before=${conversationOldestTimestamp}`;
    }

    try {
        const data = await apiGet(url);
        if (append) {
            conversationMessages = conversationMessages.concat(data);
        } else {
            conversationMessages = data;
        }
        if (conversationMessages.length > 0) {
            conversationOldestTimestamp = conversationMessages[conversationMessages.length - 1].timestamp;
        }
        renderConversationMessages();
    } catch (e) {
        showToast('Failed to load conversation');
        console.error(e);
    }
}

function renderConversationMessages() {
    const el = document.getElementById('conversation-messages');
    const loadMoreEl = document.getElementById('conversation-load-more');

    if (conversationMessages.length === 0) {
        el.innerHTML = '<div class="empty"><div class="icon">&#128172;</div><div>No messages yet</div></div>';
        loadMoreEl.style.display = 'none';
        return;
    }

    el.innerHTML = conversationMessages.map(renderMessage).join('');
    loadMoreEl.style.display = conversationMessages.length >= PAGE_SIZE ? '' : 'none';
}

function loadMoreConversationMessages() {
    if (currentConversationPeerId) {
        loadConversationMessages(currentConversationPeerId, true);
    }
}

// ---------------------------------------------------------------------------
// Compose & send
// ---------------------------------------------------------------------------
async function sendMessage() {
    const body = document.getElementById('compose-body').value.trim();
    if (!body) return;

    const kind = document.getElementById('compose-kind').value;
    const btn = document.getElementById('compose-send');
    btn.disabled = true;

    try {
        let endpoint;
        let payload;
        if (kind === 'public') {
            endpoint = '/api/messages/public';
            payload = { body };
        } else if (kind === 'direct' && currentConversationPeerId) {
            endpoint = '/api/messages/direct';
            payload = { recipient_id: currentConversationPeerId, body };
        } else {
            showToast('Unsupported message kind for compose');
            btn.disabled = false;
            return;
        }
        await apiPost(endpoint, payload);
        document.getElementById('compose-body').value = '';
        showToast('Message sent');

        // Reload conversation messages if in conversation detail view
        if (currentView === 'conversation-detail' && currentConversationPeerId) {
            loadConversationMessages(currentConversationPeerId, false);
        }
    } catch (e) {
        showToast('Send failed: ' + e.message);
    } finally {
        btn.disabled = false;
    }
}

// Send on Ctrl+Enter / Cmd+Enter
document.getElementById('compose-body').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        sendMessage();
    }
});

// ---------------------------------------------------------------------------
// Mark as read
// ---------------------------------------------------------------------------
async function markRead(messageId) {
    const el = document.querySelector(`.message[data-id="${messageId}"]`);
    if (el && el.classList.contains('unread')) {
        el.classList.remove('unread');
        try {
            await apiPost(`/api/messages/${messageId}/read`, {});
        } catch (_) {}
    }
}

// ---------------------------------------------------------------------------
// Friends management
// ---------------------------------------------------------------------------
async function loadPeers() {
    try {
        peers = await apiGet('/api/peers');
        renderFriendsList();
    } catch (e) {
        console.error('Failed to load peers:', e);
    }
}

function renderFriendsList() {
    const el = document.getElementById('friends-list');
    if (peers.length === 0) {
        el.innerHTML = '<div class="friends-empty">No friends yet</div>';
        return;
    }

    el.innerHTML = peers.map(p => {
        const displayName = p.display_name || p.peer_id.substring(0, 12) + '...';
        const onlineClass = p.online ? 'online' : '';
        const lastSeen = p.last_seen_online ? timeAgo(p.last_seen_online) : 'never';
        return `<div class="friend-item" data-peer-id="${p.peer_id}" onclick="openConversationWithPeer('${p.peer_id}')">
            <span class="online-dot ${onlineClass}"></span>
            <div class="friend-info">
                <div class="friend-name" title="${p.peer_id}">${escapeHtml(displayName)}</div>
                <div class="friend-last-seen">${p.online ? 'online' : 'last seen ' + lastSeen}</div>
            </div>
        </div>`;
    }).join('');
}

function openConversationWithPeer(peerId) {
    // Switch to conversations view and open this specific conversation
    document.querySelectorAll('.filters button').forEach(b => b.classList.remove('active'));
    document.querySelector('.filters button[data-view="conversations"]').classList.add('active');
    showConversationDetail(peerId);
}

function toggleAddFriendForm() {
    const form = document.getElementById('add-friend-form');
    form.classList.toggle('visible');
    if (!form.classList.contains('visible')) {
        document.getElementById('friend-peer-id').value = '';
        document.getElementById('friend-display-name').value = '';
        document.getElementById('friend-signing-key').value = '';
        document.getElementById('friend-enc-key').value = '';
    }
}

async function addFriend() {
    const peerId = document.getElementById('friend-peer-id').value.trim();
    const displayName = document.getElementById('friend-display-name').value.trim();
    const signingKey = document.getElementById('friend-signing-key').value.trim();
    const encKey = document.getElementById('friend-enc-key').value.trim();

    if (!peerId || !signingKey) {
        showToast('Peer ID and Signing Key are required');
        return;
    }

    try {
        const payload = {
            peer_id: peerId,
            signing_public_key: signingKey,
        };
        if (displayName) payload.display_name = displayName;
        if (encKey) payload.encryption_public_key = encKey;

        await apiPost('/api/peers', payload);
        showToast('Friend added successfully');
        toggleAddFriendForm();
        await loadPeers();
    } catch (e) {
        showToast('Failed to add friend: ' + e.message);
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------
function connectWs() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    ws = new WebSocket(`${proto}//${location.host}/api/ws`);

    ws.onmessage = (e) => {
        try {
            const event = JSON.parse(e.data);
            handleWsEvent(event);
        } catch (_) {}
    };

    ws.onclose = () => {
        setTimeout(connectWs, 3000);
    };
    ws.onerror = () => {
        ws.close();
    };
}

function handleWsEvent(event) {
    if (event.type === 'new_message') {
        // Prepend to timeline if it matches current filter
        const matchesFilter = !currentFilter || event.message_kind === currentFilter;
        const msg = {
            message_id: event.message_id,
            sender_id: event.sender_id,
            message_kind: event.message_kind,
            body: event.body,
            timestamp: event.timestamp,
            is_read: event.sender_id === myPeerId,
        };
        // Avoid duplicates
        if (!messages.find(m => m.message_id === msg.message_id)) {
            if (matchesFilter && currentView === 'timeline') {
                messages.unshift(msg);
                renderTimeline();
            }
            if (event.sender_id !== myPeerId) {
                showToast('New message from ' + event.sender_id.substring(0, 12) + '...');
            }
        }

        // If it's a direct message, refresh conversations list or conversation detail
        if (event.message_kind === 'direct') {
            if (currentView === 'conversations') {
                loadConversations();
            } else if (currentView === 'conversation-detail') {
                const otherPeerId = event.sender_id === myPeerId ? msg.recipient_id : event.sender_id;
                if (otherPeerId === currentConversationPeerId) {
                    loadConversationMessages(currentConversationPeerId, false);
                }
            }
        }
    } else if (event.type === 'message_read') {
        const el = document.querySelector(`.message[data-id="${event.message_id}"]`);
        if (el) el.classList.remove('unread');
    } else if (event.type === 'peer_online') {
        // Update peer online status
        const peer = peers.find(p => p.peer_id === event.peer_id);
        if (peer) {
            peer.online = true;
            peer.last_seen_online = Math.floor(Date.now() / 1000);
            renderFriendsList();
            const displayName = peer.display_name || peer.peer_id.substring(0, 12) + '...';
            showToast(displayName + ' is now online');
        }
    } else if (event.type === 'peer_offline') {
        // Update peer offline status
        const peer = peers.find(p => p.peer_id === event.peer_id);
        if (peer) {
            peer.online = false;
            peer.last_seen_online = Math.floor(Date.now() / 1000);
            renderFriendsList();
        }
    }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
async function init() {
    try {
        const health = await apiGet('/api/health');
        myPeerId = health.peer_id || '';
        document.getElementById('status-dot').classList.add('ok');
        const pidEl = document.getElementById('peer-id');
        pidEl.textContent = myPeerId.substring(0, 16) + '...';
        pidEl.title = myPeerId;
    } catch (_) {
        document.getElementById('status-dot').classList.add('err');
    }

    await loadPeers();
    await loadMessages(false);
    connectWs();
}

init();
