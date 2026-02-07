// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let myPeerId = '';
let currentFilter = '';
let currentView = 'timeline'; // 'timeline', 'conversations', 'conversation-detail', 'post-detail', 'profile-view', 'profile-edit'
let messages = [];
let oldestTimestamp = null;
let ws = null;
let peers = [];
let conversations = [];
let currentConversationPeerId = null;
let conversationMessages = [];
let conversationOldestTimestamp = null;
let currentPostId = null;
let pendingAttachments = []; // { file, previewUrl }
let currentPostReplies = [];
let repliesOldestTimestamp = null;
let myProfile = null;
let viewingProfileId = null;
let previousView = 'timeline';
let relayConnected = null; // null = unknown, true/false = status
const PAGE_SIZE = 50;
const MAX_ATTACHMENT_SIZE = 10 * 1024 * 1024; // 10 MB

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
async function apiPut(path, body) {
    const res = await fetch(path, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'request failed' }));
        throw new Error(err.error || 'request failed');
    }
    return res.json();
}

// ---------------------------------------------------------------------------
// Relay status banner
// ---------------------------------------------------------------------------
function updateRelayBanner(connected, relayUrl) {
    const banner = document.getElementById('relay-banner');
    const text = document.getElementById('relay-banner-text');
    if (connected === null) {
        // No relay configured
        banner.style.display = 'none';
        return;
    }
    if (connected) {
        banner.style.display = 'none';
        banner.classList.add('connected');
    } else {
        banner.style.display = '';
        banner.classList.remove('connected');
        const target = relayUrl || 'relay';
        text.textContent = 'Relay unavailable \u2014 messages cannot be sent or received. Retrying connection to ' + target + '...';
    }
    relayConnected = connected;
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
    const peer = peers.find(p => p.peer_id === m.sender_id);
    const senderLabel = isSelf ? 'You' : (peer && peer.display_name) ? peer.display_name : m.sender_id.substring(0, 12) + '...';
    const senderClass = isSelf ? 'msg-sender self' : 'msg-sender';
    const unreadClass = m.is_read ? '' : ' unread';
    const kindClass = m.message_kind || 'direct';

    // Make public messages clickable to view detail
    const isPublic = m.message_kind === 'public';
    const isReplyToSomething = !!m.reply_to;
    const clickableClass = (isPublic && !isReplyToSomething) ? ' clickable' : '';
    const onclick = (isPublic && !isReplyToSomething)
        ? `onclick="showPostDetail('${m.message_id}')"`
        : `onclick="markRead('${m.message_id}')"`;

    const attachmentsHtml = renderAttachments(m.attachments);
    const reactionsHtml = renderReactions(m);
    const replyCountHtml = renderReplyCount(m);

    return `<div class="message${unreadClass}${clickableClass}" data-id="${m.message_id}" ${onclick}>
        <div class="msg-header">
            <span class="msg-badge ${kindClass}">${kindClass}</span>
            <span class="${senderClass}" title="${m.sender_id}" onclick="event.stopPropagation();showPeerProfile('${m.sender_id}')">${senderLabel}</span>
            <span class="msg-time">${timeAgo(m.timestamp)}</span>
        </div>
        <div class="msg-body">${escapeHtml(m.body || '')}</div>
        ${attachmentsHtml}
        ${reactionsHtml}
        ${replyCountHtml}
    </div>`;
}

// ---------------------------------------------------------------------------
// Reactions (Phase 8)
// ---------------------------------------------------------------------------
function renderReactions(m) {
    const upvotes = m.upvotes || 0;
    const downvotes = m.downvotes || 0;
    const myReaction = m.my_reaction || null;

    return `<div class="msg-reactions" onclick="event.stopPropagation()">
        <button class="reaction-btn${myReaction === 'upvote' ? ' active' : ''}"
                onclick="event.stopPropagation();toggleReaction('${m.message_id}','upvote')">
            &#9650; <span class="count">${upvotes}</span>
        </button>
        <button class="reaction-btn${myReaction === 'downvote' ? ' active' : ''}"
                onclick="event.stopPropagation();toggleReaction('${m.message_id}','downvote')">
            &#9660; <span class="count">${downvotes}</span>
        </button>
    </div>`;
}

async function toggleReaction(messageId, reaction) {
    try {
        // Check current reaction
        const reactionsData = await apiGet(`/api/messages/${encodeURIComponent(messageId)}/reactions`);
        const myReaction = reactionsData.my_reaction;

        let result;
        if (myReaction === reaction) {
            // Remove existing reaction
            result = await apiDelete(`/api/messages/${encodeURIComponent(messageId)}/react`);
        } else {
            // Set new reaction
            result = await apiPost(`/api/messages/${encodeURIComponent(messageId)}/react`, { reaction });
        }

        // Update the message in our local arrays
        updateMessageReactions(messageId, result.upvotes || 0, result.downvotes || 0,
            myReaction === reaction ? null : reaction);
    } catch (e) {
        showToast('Reaction failed: ' + e.message);
    }
}

function updateMessageReactions(messageId, upvotes, downvotes, myReaction) {
    // Update in messages array
    const msg = messages.find(m => m.message_id === messageId);
    if (msg) {
        msg.upvotes = upvotes;
        msg.downvotes = downvotes;
        msg.my_reaction = myReaction;
    }
    // Update in conversation messages
    const convMsg = conversationMessages.find(m => m.message_id === messageId);
    if (convMsg) {
        convMsg.upvotes = upvotes;
        convMsg.downvotes = downvotes;
        convMsg.my_reaction = myReaction;
    }
    // Re-render the specific message reactions in place
    const el = document.querySelector(`.message[data-id="${messageId}"] .msg-reactions`);
    if (el) {
        el.outerHTML = renderReactions({ message_id: messageId, upvotes, downvotes, my_reaction: myReaction });
    }
}

// ---------------------------------------------------------------------------
// Reply count indicator (Phase 9)
// ---------------------------------------------------------------------------
function renderReplyCount(m) {
    const count = m.reply_count || 0;
    if (count === 0 || m.reply_to) return '';
    const label = count === 1 ? '1 reply' : count + ' replies';
    return `<div class="msg-reply-count" onclick="event.stopPropagation();showPostDetail('${m.message_id}')">${label}</div>`;
}

function renderAttachments(attachments) {
    if (!attachments || attachments.length === 0) return '';
    const items = attachments.map(att => {
        const url = `/api/attachments/${encodeURIComponent(att.content_hash)}`;
        const name = att.filename || att.content_hash.substring(0, 12);
        // Check if it looks like an image based on filename
        const isImage = /\.(png|jpg|jpeg|gif|webp|svg|bmp)$/i.test(name);
        if (isImage) {
            return `<a href="${url}" target="_blank"><img class="msg-attachment-img" src="${url}" alt="${escapeHtml(name)}" loading="lazy" /></a>`;
        }
        return `<a class="msg-attachment-file" href="${url}" download="${escapeHtml(name)}">
            <span class="file-icon">&#128196;</span>
            <span>${escapeHtml(name)}</span>
        </a>`;
    });
    return `<div class="msg-attachments">${items.join('')}</div>`;
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

function hideAllViews() {
    document.getElementById('timeline').style.display = 'none';
    document.getElementById('load-more').style.display = 'none';
    document.getElementById('empty-state').style.display = 'none';
    document.getElementById('conversations-list').style.display = 'none';
    document.getElementById('conversation-detail').classList.remove('visible');
    document.getElementById('post-detail').classList.remove('visible');
    document.getElementById('profile-view').classList.remove('visible');
    document.getElementById('profile-edit').classList.remove('visible');
}

function showTimelineView() {
    hideAllViews();
    document.getElementById('timeline').style.display = '';
    document.getElementById('load-more').style.display = '';
    document.getElementById('compose-kind').value = currentFilter || 'public';
    updateComposeOptions();
}

function showConversationsView() {
    hideAllViews();
    document.getElementById('conversations-list').style.display = '';
    loadConversations();
}

function showConversationDetail(peerId) {
    currentView = 'conversation-detail';
    currentConversationPeerId = peerId;
    conversationOldestTimestamp = null;
    hideAllViews();
    document.getElementById('conversation-detail').classList.add('visible');

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
    previousView = currentView;
    currentView = 'post-detail';
    currentPostId = messageId;
    currentPostReplies = [];
    repliesOldestTimestamp = null;

    hideAllViews();
    document.getElementById('post-detail').classList.add('visible');

    try {
        const post = await apiGet(`/api/messages/${encodeURIComponent(messageId)}`);
        renderPostDetail(post);
        // Mark as read when viewing
        if (!post.is_read) {
            await apiPost(`/api/messages/${encodeURIComponent(messageId)}/read`, {});
        }
        // Load replies (Phase 9)
        await loadReplies(messageId, false);
    } catch (e) {
        showToast('Failed to load post');
        console.error(e);
    }
}

function renderPostDetail(post) {
    const el = document.getElementById('post-detail-content');
    const isSelf = post.sender_id === myPeerId;
    const peer = peers.find(p => p.peer_id === post.sender_id);
    const senderLabel = isSelf ? 'You' : (peer && peer.display_name) ? peer.display_name : post.sender_id.substring(0, 12) + '...';
    const senderClass = isSelf ? 'post-sender self' : 'post-sender';
    const timestamp = new Date(post.timestamp * 1000).toLocaleString();
    const attachmentsHtml = renderAttachments(post.attachments);
    const reactionsHtml = renderReactions(post);

    el.innerHTML = `
        <div class="${senderClass}" title="${post.sender_id}" style="cursor:pointer" onclick="showPeerProfile('${post.sender_id}')">${escapeHtml(senderLabel)}</div>
        <div class="post-time">${timestamp}</div>
        <div class="post-body">${escapeHtml(post.body || '')}</div>
        ${attachmentsHtml}
        ${reactionsHtml}
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
    if (!body && pendingAttachments.length === 0) return;

    const kind = document.getElementById('compose-kind').value;
    const btn = document.getElementById('compose-send');
    btn.disabled = true;

    try {
        // Upload attachments first
        const uploadedAttachments = [];
        for (const att of pendingAttachments) {
            const result = await uploadAttachment(att.file);
            uploadedAttachments.push({
                content_hash: result.content_hash,
                filename: att.file.name,
            });
        }

        let endpoint;
        let payload;
        if (kind === 'public') {
            endpoint = '/api/messages/public';
            payload = { body: body || '', attachments: uploadedAttachments };
        } else if (kind === 'direct' && currentConversationPeerId) {
            endpoint = '/api/messages/direct';
            payload = { recipient_id: currentConversationPeerId, body: body || '', attachments: uploadedAttachments };
        } else {
            showToast('Unsupported message kind for compose');
            btn.disabled = false;
            return;
        }
        await apiPost(endpoint, payload);
        document.getElementById('compose-body').value = '';
        clearAttachments();
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

// ---------------------------------------------------------------------------
// Attachment management (Phase 7)
// ---------------------------------------------------------------------------
function handleFileSelect(files) {
    for (const file of files) {
        if (file.size > MAX_ATTACHMENT_SIZE) {
            showToast('File "' + file.name + '" exceeds 10 MB limit');
            continue;
        }
        const previewUrl = file.type.startsWith('image/') ? URL.createObjectURL(file) : null;
        pendingAttachments.push({ file, previewUrl });
    }
    renderAttachmentPreviews();
    // Reset input so the same file can be re-selected
    document.getElementById('attachment-input').value = '';
}

function removeAttachment(index) {
    const att = pendingAttachments[index];
    if (att.previewUrl) URL.revokeObjectURL(att.previewUrl);
    pendingAttachments.splice(index, 1);
    renderAttachmentPreviews();
}

function clearAttachments() {
    for (const att of pendingAttachments) {
        if (att.previewUrl) URL.revokeObjectURL(att.previewUrl);
    }
    pendingAttachments = [];
    renderAttachmentPreviews();
}

function renderAttachmentPreviews() {
    const el = document.getElementById('attachment-previews');
    if (pendingAttachments.length === 0) {
        el.innerHTML = '';
        return;
    }
    el.innerHTML = pendingAttachments.map((att, i) => {
        const imgTag = att.previewUrl
            ? '<img src="' + att.previewUrl + '" alt="" />'
            : '<span style="font-size:1.5rem;">&#128196;</span>';
        return '<div class="attachment-preview">'
            + imgTag
            + '<div class="att-info">'
            + '<div class="att-name" title="' + escapeHtml(att.file.name) + '">' + escapeHtml(att.file.name) + '</div>'
            + '<div class="att-size">' + formatBytes(att.file.size) + '</div>'
            + '</div>'
            + '<button class="att-remove" onclick="removeAttachment(' + i + ')">x</button>'
            + '</div>';
    }).join('');
}

function formatBytes(bytes) {
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
}

async function uploadAttachment(file) {
    const formData = new FormData();
    formData.append('file', file);
    const res = await fetch('/api/attachments', { method: 'POST', body: formData });
    if (!res.ok) {
        const err = await res.json().catch(() => ({ error: 'upload failed' }));
        throw new Error(err.error || 'upload failed');
    }
    return res.json();
}

// Drag and drop on compose box
(function setupDragDrop() {
    const compose = document.getElementById('compose-box');
    if (!compose) return;
    compose.addEventListener('dragover', (e) => {
        e.preventDefault();
        compose.classList.add('compose-drop-active');
    });
    compose.addEventListener('dragleave', () => {
        compose.classList.remove('compose-drop-active');
    });
    compose.addEventListener('drop', (e) => {
        e.preventDefault();
        compose.classList.remove('compose-drop-active');
        if (e.dataTransfer.files.length > 0) {
            handleFileSelect(e.dataTransfer.files);
        }
    });
})();

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
                <div class="friend-name" title="${p.peer_id}" onclick="event.stopPropagation();showPeerProfile('${p.peer_id}')">${escapeHtml(displayName)}</div>
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
        document.getElementById('friend-message').value = '';
    }
}

async function sendFriendRequest() {
    const peerId = document.getElementById('friend-peer-id').value.trim();
    const message = document.getElementById('friend-message').value.trim();

    if (!peerId) {
        showToast('Peer ID is required');
        return;
    }

    try {
        const payload = { peer_id: peerId };
        if (message) payload.message = message;

        await apiPost('/api/friend-requests', payload);
        showToast('Friend request sent');
        toggleAddFriendForm();
        await loadFriendRequests();
    } catch (e) {
        showToast('Failed to send request: ' + e.message);
    }
}

// ---------------------------------------------------------------------------
// Friend Requests
// ---------------------------------------------------------------------------
let friendRequests = [];
let currentFrTab = 'pending';

function toggleFriendRequests() {
    const content = document.getElementById('friend-requests-content');
    content.classList.toggle('visible');
    if (content.classList.contains('visible')) {
        loadFriendRequests();
    }
}

function switchFrTab(tab) {
    currentFrTab = tab;
    document.querySelectorAll('.fr-tab').forEach(b => b.classList.remove('active'));
    document.querySelector(`.fr-tab[data-fr-tab="${tab}"]`).classList.add('active');
    renderFriendRequests();
}

async function loadFriendRequests() {
    try {
        friendRequests = await apiGet('/api/friend-requests');
        updateFrBadge();
        renderFriendRequests();
    } catch (e) {
        console.error('Failed to load friend requests:', e);
    }
}

function updateFrBadge() {
    const pendingIncoming = friendRequests.filter(r => r.status === 'pending' && r.direction === 'incoming');
    const pendingOutgoing = friendRequests.filter(r => r.status === 'pending' && r.direction === 'outgoing');
    const totalPending = pendingIncoming.length + pendingOutgoing.length;
    const badge = document.getElementById('fr-pending-badge');
    if (totalPending > 0) {
        badge.textContent = totalPending;
        badge.style.display = '';
    } else {
        badge.style.display = 'none';
    }
}

function renderFriendRequests() {
    const el = document.getElementById('friend-requests-list');
    const showHidden = document.getElementById('fr-show-hidden').checked;

    let filtered;
    if (currentFrTab === 'pending') {
        filtered = friendRequests.filter(r => r.status === 'pending');
    } else {
        // History: accepted + optionally blocked/ignored
        filtered = friendRequests.filter(r => {
            if (r.status === 'accepted') return true;
            if (showHidden && (r.status === 'blocked' || r.status === 'ignored')) return true;
            return false;
        });
    }

    if (filtered.length === 0) {
        const msg = currentFrTab === 'pending' ? 'No pending requests' : 'No request history';
        el.innerHTML = '<div class="fr-empty">' + msg + '</div>';
        return;
    }

    el.innerHTML = filtered.map(r => {
        const isIncoming = r.direction === 'incoming';
        const otherPeerId = isIncoming ? r.from_peer_id : r.to_peer_id;
        const shortId = otherPeerId.substring(0, 12) + '...';
        const dirLabel = isIncoming ? 'From' : 'To';
        const messageHtml = r.message
            ? '<div class="fr-message">' + escapeHtml(r.message) + '</div>'
            : '';
        const timeStr = timeAgo(r.created_at);

        let statusHtml = '';
        if (r.status === 'pending' && isIncoming) {
            statusHtml = '<div class="fr-actions">'
                + '<button class="fr-accept" onclick="acceptFriendRequest(' + r.id + ')">Accept</button>'
                + '<button class="fr-ignore" onclick="ignoreFriendRequest(' + r.id + ')">Ignore</button>'
                + '<button class="fr-block" onclick="blockFriendRequest(' + r.id + ')">Block</button>'
                + '</div>';
        } else if (r.status === 'pending' && !isIncoming) {
            statusHtml = '<div class="fr-status pending">Pending</div>';
        } else {
            statusHtml = '<div class="fr-status ' + r.status + '">' + r.status.charAt(0).toUpperCase() + r.status.slice(1) + '</div>';
        }

        return '<div class="fr-item" data-status="' + r.status + '">'
            + '<div class="fr-item-header">'
            + '<span class="fr-direction">' + dirLabel + '</span> '
            + '<span class="fr-peer" title="' + escapeHtml(otherPeerId) + '">' + escapeHtml(shortId) + '</span>'
            + '<span class="fr-time">' + timeStr + '</span>'
            + '</div>'
            + messageHtml
            + statusHtml
            + '</div>';
    }).join('');
}

async function acceptFriendRequest(id) {
    try {
        await apiPost('/api/friend-requests/' + id + '/accept', {});
        showToast('Friend request accepted');
        await loadFriendRequests();
        await loadPeers();
    } catch (e) {
        showToast('Failed to accept: ' + e.message);
    }
}

async function ignoreFriendRequest(id) {
    try {
        await apiPost('/api/friend-requests/' + id + '/ignore', {});
        showToast('Friend request ignored');
        await loadFriendRequests();
    } catch (e) {
        showToast('Failed to ignore: ' + e.message);
    }
}

async function blockFriendRequest(id) {
    try {
        await apiPost('/api/friend-requests/' + id + '/block', {});
        showToast('Peer blocked');
        await loadFriendRequests();
    } catch (e) {
        showToast('Failed to block: ' + e.message);
    }
}

// ---------------------------------------------------------------------------
// Copy Peer ID
// ---------------------------------------------------------------------------
function copyPeerId() {
    if (!myPeerId) return;
    navigator.clipboard.writeText(myPeerId).then(() => {
        showToast('Peer ID copied to clipboard');
    }).catch(() => {
        // Fallback for non-HTTPS contexts
        const ta = document.createElement('textarea');
        ta.value = myPeerId;
        ta.style.position = 'fixed';
        ta.style.left = '-9999px';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        document.body.removeChild(ta);
        showToast('Peer ID copied to clipboard');
    });
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
    } else if (event.type === 'friend_request_received') {
        showToast('New friend request from ' + event.from_peer_id.substring(0, 12) + '...');
        loadFriendRequests();
    } else if (event.type === 'friend_request_accepted') {
        showToast('Friend request accepted by ' + event.from_peer_id.substring(0, 12) + '...');
        loadFriendRequests();
        loadPeers();
    } else if (event.type === 'relay_status') {
        updateRelayBanner(event.connected, event.relay_url);
        if (event.connected && relayConnected === false) {
            showToast('Relay reconnected');
        } else if (!event.connected && relayConnected !== false) {
            showToast('Relay disconnected');
        }
    }
}

// ---------------------------------------------------------------------------
// Replies (Phase 9)
// ---------------------------------------------------------------------------
async function loadReplies(messageId, append) {
    let url = `/api/messages/${encodeURIComponent(messageId)}/replies?limit=${PAGE_SIZE}`;
    if (append && repliesOldestTimestamp) {
        url += `&before=${repliesOldestTimestamp}`;
    }

    try {
        const data = await apiGet(url);
        if (append) {
            currentPostReplies = currentPostReplies.concat(data);
        } else {
            currentPostReplies = data;
        }
        if (currentPostReplies.length > 0) {
            repliesOldestTimestamp = currentPostReplies[currentPostReplies.length - 1].timestamp + 1;
        }
        renderReplies();
    } catch (e) {
        console.error('Failed to load replies:', e);
    }
}

function renderReplies() {
    const el = document.getElementById('post-replies');
    const loadMoreEl = document.getElementById('replies-load-more');

    if (currentPostReplies.length === 0) {
        el.innerHTML = '';
        loadMoreEl.style.display = 'none';
        return;
    }

    el.innerHTML = currentPostReplies.map(renderReplyItem).join('');
    loadMoreEl.style.display = currentPostReplies.length >= PAGE_SIZE ? '' : 'none';
}

function renderReplyItem(m) {
    const isSelf = m.sender_id === myPeerId;
    const peer = peers.find(p => p.peer_id === m.sender_id);
    const senderLabel = isSelf ? 'You' : (peer && peer.display_name) ? peer.display_name : m.sender_id.substring(0, 12) + '...';
    const senderClass = isSelf ? 'msg-sender self' : 'msg-sender';
    const attachmentsHtml = renderAttachments(m.attachments);
    const reactionsHtml = renderReactions(m);

    return `<div class="reply-item" data-id="${m.message_id}">
        <div class="msg-header">
            <span class="${senderClass}" title="${m.sender_id}" onclick="showPeerProfile('${m.sender_id}')">${senderLabel}</span>
            <span class="msg-time">${timeAgo(m.timestamp)}</span>
        </div>
        <div class="msg-body">${escapeHtml(m.body || '')}</div>
        ${attachmentsHtml}
        ${reactionsHtml}
    </div>`;
}

function loadMoreReplies() {
    if (currentPostId) {
        loadReplies(currentPostId, true);
    }
}

async function sendReply() {
    const body = document.getElementById('reply-body').value.trim();
    if (!body || !currentPostId) return;

    try {
        await apiPost(`/api/messages/${encodeURIComponent(currentPostId)}/reply`, { body });
        document.getElementById('reply-body').value = '';
        showToast('Reply sent');
        // Reload replies
        await loadReplies(currentPostId, false);
    } catch (e) {
        showToast('Reply failed: ' + e.message);
    }
}

// Send reply on Ctrl+Enter in reply box
document.getElementById('reply-body').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        sendReply();
    }
});

// ---------------------------------------------------------------------------
// Profiles (Phase 10)
// ---------------------------------------------------------------------------
async function loadMyProfile() {
    try {
        myProfile = await apiGet('/api/profile');
        renderMyProfileCard();
    } catch (e) {
        console.error('Failed to load profile:', e);
    }
}

function renderMyProfileCard() {
    const nameEl = document.getElementById('my-profile-name');
    const bioEl = document.getElementById('my-profile-bio');
    const avatarEl = document.getElementById('my-profile-avatar');

    if (myProfile && myProfile.display_name) {
        nameEl.textContent = myProfile.display_name;
    } else {
        nameEl.textContent = myPeerId ? myPeerId.substring(0, 12) + '...' : 'Me';
    }

    if (myProfile && myProfile.bio) {
        bioEl.textContent = myProfile.bio.substring(0, 60);
    } else {
        bioEl.textContent = 'Click to edit profile';
    }

    if (myProfile && myProfile.avatar_hash) {
        avatarEl.innerHTML = `<img src="/api/attachments/${encodeURIComponent(myProfile.avatar_hash)}" alt="" />`;
    } else {
        const initial = (myProfile && myProfile.display_name) ? myProfile.display_name[0].toUpperCase() : '?';
        avatarEl.textContent = initial;
    }
}

function showMyProfile() {
    previousView = currentView;
    currentView = 'profile-edit';
    viewingProfileId = myPeerId;

    hideAllViews();
    document.getElementById('profile-edit').classList.add('visible');
    document.getElementById('compose-box').style.display = 'none';

    // Show peer ID
    const editPeerId = document.getElementById('profile-edit-peer-id');
    if (editPeerId) editPeerId.textContent = myPeerId;

    // Pre-fill form
    document.getElementById('profile-display-name').value = (myProfile && myProfile.display_name) || '';
    document.getElementById('profile-bio').value = (myProfile && myProfile.bio) || '';
    document.getElementById('profile-avatar-hash').value = (myProfile && myProfile.avatar_hash) || '';
}

async function saveProfile() {
    const displayName = document.getElementById('profile-display-name').value.trim();
    const bio = document.getElementById('profile-bio').value.trim();
    const avatarHash = document.getElementById('profile-avatar-hash').value.trim();

    try {
        myProfile = await apiPut('/api/profile', {
            display_name: displayName || null,
            bio: bio || null,
            avatar_hash: avatarHash || null,
        });
        renderMyProfileCard();
        showToast('Profile saved');
        backFromProfile();
    } catch (e) {
        showToast('Failed to save profile: ' + e.message);
    }
}

async function showPeerProfile(peerId) {
    if (peerId === myPeerId) {
        showMyProfile();
        return;
    }

    previousView = currentView;
    currentView = 'profile-view';
    viewingProfileId = peerId;

    hideAllViews();
    document.getElementById('profile-view').classList.add('visible');
    document.getElementById('compose-box').style.display = 'none';

    try {
        const profile = await apiGet(`/api/peers/${encodeURIComponent(peerId)}/profile`);
        renderPeerProfile(profile, peerId);
    } catch (e) {
        const el = document.getElementById('profile-view-content');
        const peer = peers.find(p => p.peer_id === peerId);
        const name = (peer && peer.display_name) || peerId.substring(0, 12) + '...';
        el.innerHTML = `
            <div class="profile-view-header">
                <div class="profile-view-avatar">${name[0].toUpperCase()}</div>
                <div>
                    <div class="profile-view-name">${escapeHtml(name)}</div>
                    <div class="profile-view-id">${escapeHtml(peerId)}</div>
                </div>
            </div>
            <div class="profile-view-bio">No profile available.</div>
        `;
    }
}

function renderPeerProfile(profile, peerId) {
    const el = document.getElementById('profile-view-content');
    const name = profile.display_name || peerId.substring(0, 12) + '...';
    const initial = name[0].toUpperCase();

    let avatarHtml;
    if (profile.avatar_hash) {
        avatarHtml = `<div class="profile-view-avatar"><img src="/api/attachments/${encodeURIComponent(profile.avatar_hash)}" alt="" /></div>`;
    } else {
        avatarHtml = `<div class="profile-view-avatar">${initial}</div>`;
    }

    const bioHtml = profile.bio
        ? `<div class="profile-view-bio">${escapeHtml(profile.bio)}</div>`
        : '<div class="profile-view-bio" style="color:#666">No bio set.</div>';

    // Render public fields
    let fieldsHtml = '';
    const publicFields = profile.public_fields || {};
    const friendsFields = profile.friends_fields || {};
    const allFields = { ...publicFields, ...friendsFields };
    const fieldKeys = Object.keys(allFields);
    if (fieldKeys.length > 0) {
        fieldsHtml = '<dl class="profile-view-fields">';
        for (const key of fieldKeys) {
            fieldsHtml += `<dt>${escapeHtml(key)}</dt><dd>${escapeHtml(String(allFields[key]))}</dd>`;
        }
        fieldsHtml += '</dl>';
    }

    // Add button to open conversation if they are a peer
    const peer = peers.find(p => p.peer_id === peerId);
    const actionHtml = peer
        ? `<div style="margin-top:1rem;"><button class="btn-primary" style="background:#5566cc;border:none;color:#fff;padding:0.5rem 1rem;border-radius:6px;cursor:pointer;font-size:0.85rem;" onclick="backFromProfile();openConversationWithPeer('${peerId}')">Send Message</button></div>`
        : '';

    const peerIdCopyHtml = `<div class="profile-view-id-row">
        <code class="profile-view-id-full">${escapeHtml(peerId)}</code>
        <button class="copy-btn" onclick="navigator.clipboard.writeText('${escapeHtml(peerId)}').then(()=>showToast('Peer ID copied'))">Copy</button>
    </div>`;

    el.innerHTML = `
        <div class="profile-view-header">
            ${avatarHtml}
            <div>
                <div class="profile-view-name">${escapeHtml(name)}</div>
                <div class="profile-view-id">${escapeHtml(peerId)}</div>
            </div>
        </div>
        ${peerIdCopyHtml}
        ${bioHtml}
        ${fieldsHtml}
        ${actionHtml}
    `;
}

function backFromProfile() {
    currentView = previousView || 'timeline';
    viewingProfileId = null;
    document.getElementById('profile-view').classList.remove('visible');
    document.getElementById('profile-edit').classList.remove('visible');
    document.getElementById('compose-box').style.display = '';

    // Restore previous view
    if (currentView === 'timeline') {
        showTimelineView();
        loadMessages(false);
    } else if (currentView === 'conversations') {
        showConversationsView();
    } else if (currentView === 'conversation-detail' && currentConversationPeerId) {
        showConversationDetail(currentConversationPeerId);
    } else if (currentView === 'post-detail' && currentPostId) {
        // Re-show post detail
        hideAllViews();
        document.getElementById('post-detail').classList.add('visible');
    } else {
        showTimelineView();
        loadMessages(false);
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

        // Show relay status from health check
        if (health.relay && health.relay !== 'none') {
            updateRelayBanner(health.relay_connected, health.relay);
        }
    } catch (_) {
        document.getElementById('status-dot').classList.add('err');
    }

    // Display full peer ID in sidebar
    const peerIdFull = document.getElementById('peer-id-full');
    if (peerIdFull) peerIdFull.textContent = myPeerId;

    await loadPeers();
    await loadMyProfile();
    await loadFriendRequests();
    await loadMessages(false);
    connectWs();
}

init();
