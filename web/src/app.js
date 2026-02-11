// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let myPeerId = '';
let currentFilter = 'public'; // 'public' or 'friend_group'
let currentView = 'timeline'; // 'timeline', 'friends', 'conversation-detail', 'post-detail', 'profile-view', 'profile-edit'
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
let notifications = []; // Phase 11
let unseenNotificationCount = 0; // Phase 11 - resets to 0 when bell is clicked
let notificationPanelOpen = false; // Phase 11
let pendingFriendRequestHighlight = null; // peer ID to highlight on friends page
const PAGE_SIZE = 50;
const COMMENTS_PAGE_SIZE = 100;
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
// Hash-based URL routing
// ---------------------------------------------------------------------------
function navigateTo(route) {
    if (route === 'timeline') {
        window.location.hash = '#/';
    } else {
        window.location.hash = '#/' + route;
    }
}

function handleRoute() {
    const hash = window.location.hash || '#/';
    const parts = hash.replace('#/', '').split('/');
    const route = parts[0] || '';

    if (route === 'post' && parts[1]) {
        // Post detail: #/post/{messageId}
        const messageId = decodeURIComponent(parts.slice(1).join('/'));
        showPostDetail(messageId, true);
    } else if (route === 'peer' && parts[1]) {
        // DM conversation: #/peer/{peerId}
        const peerId = decodeURIComponent(parts.slice(1).join('/'));
        showConversationDetail(peerId, true);
    } else if (route === 'profile') {
        showMyProfile(true);
    } else if (route === 'friends') {
        showFriendsView(true);
    } else {
        // Default: timeline
        currentView = 'timeline';
        currentPostId = null;
        currentConversationPeerId = null;
        showTimelineView();
        loadMessages(false);
    }
}

window.addEventListener('hashchange', handleRoute);

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

async function retryRelayConnection() {
    const btn = document.getElementById('relay-retry-btn');
    if (btn) { btn.disabled = true; btn.textContent = 'Retrying...'; }
    try {
        await apiPost('/api/sync', {});
    } catch (_) {
        // Status update arrives via WebSocket; ignore fetch errors here
    } finally {
        if (btn) { btn.disabled = false; btn.textContent = 'Retry now'; }
    }
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

function exactTime(ts) {
    return new Date(ts * 1000).toLocaleString(undefined, {
        year: 'numeric', month: 'short', day: 'numeric',
        hour: '2-digit', minute: '2-digit', second: '2-digit'
    });
}

function msgTimeTitle(m) {
    const sent = exactTime(m.timestamp);
    const recv = exactTime(m.received_at);
    return m.received_at && m.received_at !== m.timestamp
        ? `Sent: ${sent}\nReceived: ${recv}`
        : `Sent: ${sent}`;
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

    // Make public and group posts clickable to view detail (only root posts)
    const isClickable = (m.message_kind === 'public' || m.message_kind === 'friend_group') && !m.reply_to;
    const clickableClass = isClickable ? ' clickable' : '';
    const onclick = isClickable
        ? `onclick="navigateTo('post/${encodeURIComponent(m.message_id)}')"`
        : `onclick="markRead('${m.message_id}')"`;

    const attachmentsHtml = renderAttachments(m.attachments);
    const reactionsHtml = renderReactions(m);
    const commentCountHtml = renderCommentCount(m);

    return `<div class="message${unreadClass}${clickableClass}" data-id="${m.message_id}" ${onclick}>
        <div class="msg-header">
            <span class="msg-badge ${kindClass}">${kindClass}</span>
            <span class="${senderClass}" title="${m.sender_id}" onclick="event.stopPropagation();showPeerProfile('${m.sender_id}')">${senderLabel}</span>
            <span class="msg-time" title="${msgTimeTitle(m)}">${timeAgo(m.timestamp)}</span>
        </div>
        <div class="msg-body">${escapeHtml(m.body || '')}</div>
        ${attachmentsHtml}
        ${reactionsHtml}
        ${commentCountHtml}
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
    const el = document.querySelector(`.message[data-id="${messageId}"] .msg-reactions, .dm-message[data-id="${messageId}"] .msg-reactions`);
    if (el) {
        el.outerHTML = renderReactions({ message_id: messageId, upvotes, downvotes, my_reaction: myReaction });
    }
    // Also update the post detail view if it's showing this post
    if (currentView === 'post-detail' && currentPostId === messageId) {
        const detailEl = document.querySelector('#post-detail-content .msg-reactions');
        if (detailEl) {
            detailEl.outerHTML = renderReactions({ message_id: messageId, upvotes, downvotes, my_reaction: myReaction });
        }
    }
}

// ---------------------------------------------------------------------------
// Comment count indicator
// ---------------------------------------------------------------------------
function renderCommentCount(m) {
    const count = m.reply_count || 0;
    // Only show on root posts (not on replies/comments themselves)
    if (count === 0 || m.reply_to) return '';
    const label = count === 1 ? '1 comment' : count + ' comments';
    return `<div class="msg-reply-count" onclick="event.stopPropagation();navigateTo('post/${encodeURIComponent(m.message_id)}')">${label}</div>`;
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
    // Timeline only shows public or friend_group posts
    if (currentFilter) url += `&kind=${currentFilter}`;
    if (append && oldestTimestamp) url += `&before=${oldestTimestamp}`;

    try {
        const data = await apiGet(url);
        // Filter out replies/comments - only show root posts on the timeline
        const rootPosts = data.filter(m => !m.reply_to);
        if (append) {
            messages = messages.concat(rootPosts);
        } else {
            messages = rootPosts;
        }
        if (data.length > 0) {
            // Use the oldest timestamp from the raw data for pagination
            oldestTimestamp = data[data.length - 1].timestamp;
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

        currentView = 'timeline';
        currentFilter = btn.dataset.kind || 'public';
        oldestTimestamp = null;
        showTimelineView();
        loadMessages(false);
        // Update hash without triggering route
        window.location.hash = '#/';
    });
});

function hideAllViews() {
    document.getElementById('timeline').style.display = 'none';
    document.getElementById('load-more').style.display = 'none';
    document.getElementById('empty-state').style.display = 'none';
    document.getElementById('conversation-detail').classList.remove('visible');
    document.getElementById('post-detail').classList.remove('visible');
    document.getElementById('profile-view').classList.remove('visible');
    document.getElementById('profile-edit').classList.remove('visible');
    document.getElementById('friends-view').classList.remove('visible');
    document.getElementById('compose-box').style.display = '';
    // Hide filters on non-timeline views
    document.querySelector('.filters').style.display = '';
}

function showTimelineView() {
    hideAllViews();
    document.getElementById('timeline').style.display = '';
    document.getElementById('load-more').style.display = '';
    document.querySelector('.filters').style.display = '';
    document.getElementById('compose-box').style.display = '';
    updateComposeForTimeline();
}

function updateComposeForTimeline() {
    const select = document.getElementById('compose-kind');
    select.innerHTML = '';

    if (currentFilter === 'friend_group') {
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
// Conversation detail (DMs with a friend)
// ---------------------------------------------------------------------------
function showConversationDetail(peerId, fromRoute) {
    currentView = 'conversation-detail';
    currentConversationPeerId = peerId;
    conversationOldestTimestamp = null;

    hideAllViews();
    document.querySelector('.filters').style.display = 'none';
    // Hide the main compose box; DM view has its own compose
    document.getElementById('compose-box').style.display = 'none';
    document.getElementById('conversation-detail').classList.add('visible');

    const peer = peers.find(p => p.peer_id === peerId);
    const peerName = peer?.display_name || peerId.substring(0, 12) + '...';
    document.getElementById('conversation-peer-name').textContent = peerName;

    renderDmPeerInfo(peerId);
    loadConversationMessages(peerId, false);

    // Update URL if not already navigating from route
    if (!fromRoute) {
        window.location.hash = '#/peer/' + encodeURIComponent(peerId);
    }
}

async function renderDmPeerInfo(peerId) {
    const el = document.getElementById('dm-peer-info');
    const peer = peers.find(p => p.peer_id === peerId);
    const name = peer?.display_name || peerId.substring(0, 12) + '...';
    const initial = name[0].toUpperCase();
    const onlineText = peer?.online ? 'Online' : (peer?.last_seen_online ? 'Last seen ' + timeAgo(peer.last_seen_online) : 'Offline');
    const onlineClass = peer?.online ? ' online' : '';

    let profile = null;
    try {
        profile = await apiGet(`/api/peers/${encodeURIComponent(peerId)}/profile`);
    } catch (_) {}

    const avatarContent = profile?.avatar_hash
        ? `<img src="/api/attachments/${encodeURIComponent(profile.avatar_hash)}" alt="" />`
        : initial;
    const bioHtml = profile?.bio
        ? `<div class="dm-peer-bio">${escapeHtml(profile.bio)}</div>`
        : '';

    el.innerHTML = `
        <div class="dm-peer-header">
            <div class="dm-peer-avatar">${avatarContent}</div>
            <div>
                <div class="dm-peer-name">${escapeHtml(name)}</div>
                <div class="dm-peer-id">${escapeHtml(peerId.substring(0, 24))}...</div>
                <div class="dm-peer-status${onlineClass}">${onlineText}</div>
            </div>
        </div>
        ${bioHtml}
    `;
}

function openConversationWithPeer(peerId) {
    navigateTo('peer/' + encodeURIComponent(peerId));
}

// ---------------------------------------------------------------------------
// Post detail view
// ---------------------------------------------------------------------------
async function showPostDetail(messageId, fromRoute) {
    previousView = currentView;
    currentView = 'post-detail';
    currentPostId = messageId;
    currentPostReplies = [];
    repliesOldestTimestamp = null;

    hideAllViews();
    document.querySelector('.filters').style.display = 'none';
    document.getElementById('compose-box').style.display = 'none';
    document.getElementById('post-detail').classList.add('visible');

    // Update URL if not already navigating from route
    if (!fromRoute) {
        window.location.hash = '#/post/' + encodeURIComponent(messageId);
    }

    try {
        const post = await apiGet(`/api/messages/${encodeURIComponent(messageId)}`);
        renderPostDetail(post);
        // Mark as read when viewing
        if (!post.is_read) {
            await apiPost(`/api/messages/${encodeURIComponent(messageId)}/read`, {});
        }
        // Load comments (up to 100)
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
    const senderClass = isSelf ? 'msg-sender self' : 'msg-sender';
    const kindClass = post.message_kind || 'public';
    const attachmentsHtml = renderAttachments(post.attachments);
    const reactionsHtml = renderReactions(post);

    el.innerHTML = `
        <div class="msg-header" style="margin-bottom:0.75rem;">
            <span class="msg-badge ${kindClass}">${kindClass}</span>
            <span class="${senderClass}" title="${post.sender_id}" style="cursor:pointer" onclick="showPeerProfile('${post.sender_id}')">${escapeHtml(senderLabel)}</span>
            <span class="msg-time" title="${msgTimeTitle(post)}">${timeAgo(post.timestamp)}</span>
        </div>
        <div class="post-body">${escapeHtml(post.body || '')}</div>
        ${attachmentsHtml}
        ${reactionsHtml}
    `;
}

function updateComposeOptions() {
    // Kept for backward compatibility but no longer used for DM
    const select = document.getElementById('compose-kind');
    select.innerHTML = '';

    if (currentFilter === 'friend_group') {
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
// Conversations (DM messages with a peer)
// ---------------------------------------------------------------------------
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
        renderConversationMessages(append);
    } catch (e) {
        showToast('Failed to load conversation');
        console.error(e);
    }
}

function renderConversationMessages(wasAppend) {
    const el = document.getElementById('conversation-messages');
    const loadMoreEl = document.getElementById('conversation-load-more');
    const messagesArea = document.getElementById('dm-messages-area');

    // Capture scroll state before re-rendering (for append / load-older case)
    const prevScrollHeight = messagesArea ? messagesArea.scrollHeight : 0;
    const prevScrollTop = messagesArea ? messagesArea.scrollTop : 0;

    if (conversationMessages.length === 0) {
        el.innerHTML = '<div class="empty"><div class="icon">&#128172;</div><div>No messages yet. Send the first message!</div></div>';
        loadMoreEl.style.display = 'none';
    } else {
        // Render in chronological order (oldest first, newest at bottom)
        el.innerHTML = conversationMessages.slice().reverse().map(renderDmMessage).join('');
        loadMoreEl.style.display = conversationMessages.length >= PAGE_SIZE ? '' : 'none';
    }

    // Always render the DM compose below messages
    renderDmCompose();

    // Scroll to bottom on fresh load; preserve position when loading older messages
    requestAnimationFrame(() => {
        if (messagesArea) {
            if (wasAppend) {
                messagesArea.scrollTop = prevScrollTop + messagesArea.scrollHeight - prevScrollHeight;
            } else {
                messagesArea.scrollTop = messagesArea.scrollHeight;
            }
        }
    });
}

function renderDmMessage(m) {
    const isSelf = m.sender_id === myPeerId;
    const peer = peers.find(p => p.peer_id === m.sender_id);
    const senderLabel = isSelf ? 'You' : (peer && peer.display_name) ? peer.display_name : m.sender_id.substring(0, 12) + '...';
    const bubbleClass = isSelf ? 'sent' : 'received';
    const unreadClass = m.is_read ? '' : ' unread';
    const attachmentsHtml = renderAttachments(m.attachments);
    const senderHtml = !isSelf ? `<div class="dm-bubble-sender">${escapeHtml(senderLabel)}</div>` : '';

    return `<div class="dm-message ${bubbleClass}${unreadClass}" data-id="${m.message_id}" onclick="markRead('${m.message_id}')">
        <div class="dm-bubble">
            ${senderHtml}
            <div class="msg-body">${escapeHtml(m.body || '')}</div>
            ${attachmentsHtml}
            <div class="dm-bubble-meta" title="${msgTimeTitle(m)}">${timeAgo(m.timestamp)}</div>
        </div>
    </div>`;
}

function renderDmCompose() {
    // Check if DM compose already exists
    let existing = document.getElementById('dm-compose-box');
    if (existing) return;

    const container = document.getElementById('conversation-detail');
    const composeDiv = document.createElement('div');
    composeDiv.className = 'dm-compose';
    composeDiv.id = 'dm-compose-box';
    composeDiv.innerHTML = `
        <textarea id="dm-compose-body" placeholder="Write a message... (Enter to send, Shift+Enter for newline)"></textarea>
        <div class="dm-compose-actions">
            <button id="dm-compose-send" onclick="sendDmMessage()">Send</button>
        </div>
    `;
    // Append after messages (load-more is now at the top of the conversation)
    container.appendChild(composeDiv);

    // Enter to send; Shift+Enter for newline
    document.getElementById('dm-compose-body').addEventListener('keydown', (e) => {
        if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            sendDmMessage();
        }
    });
}

async function sendDmMessage() {
    const bodyEl = document.getElementById('dm-compose-body');
    if (!bodyEl) return;
    const body = bodyEl.value.trim();
    if (!body || !currentConversationPeerId) return;

    const btn = document.getElementById('dm-compose-send');
    btn.disabled = true;

    try {
        await apiPost('/api/messages/direct', {
            recipient_id: currentConversationPeerId,
            body: body,
            attachments: [],
        });
        bodyEl.value = '';
        showToast('Message sent');
        loadConversationMessages(currentConversationPeerId, false);
    } catch (e) {
        showToast('Send failed: ' + e.message);
    } finally {
        btn.disabled = false;
    }
}

function loadMoreConversationMessages() {
    if (currentConversationPeerId) {
        loadConversationMessages(currentConversationPeerId, true);
    }
}

// ---------------------------------------------------------------------------
// Compose & send (for timeline: public/group posts)
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
        } else {
            showToast('Unsupported message kind for compose');
            btn.disabled = false;
            return;
        }
        await apiPost(endpoint, payload);
        document.getElementById('compose-body').value = '';
        clearAttachments();
        showToast('Message sent');
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
    const el = document.querySelector(`.message[data-id="${messageId}"], .dm-message[data-id="${messageId}"]`);
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
        // Use last_seen_online, falling back to added_at (time of friend request/acceptance)
        const lastSeenTs = p.last_seen_online || p.added_at;
        const lastSeen = lastSeenTs ? timeAgo(lastSeenTs) : 'never';
        return `<div class="friend-item" data-peer-id="${p.peer_id}" onclick="openConversationWithPeer('${p.peer_id}')">
            <span class="online-dot ${onlineClass}"></span>
            <div class="friend-info">
                <div class="friend-name" title="${p.peer_id}">${escapeHtml(displayName)}</div>
                <div class="friend-last-seen">${p.online ? 'online' : 'last seen ' + lastSeen}</div>
            </div>
        </div>`;
    }).join('');
}

function toggleAddFriendForm() {
    const form = document.getElementById('add-friend-form');
    form.classList.toggle('visible');
    if (!form.classList.contains('visible')) {
        document.getElementById('friend-peer-id').value = '';
        document.getElementById('friend-message').value = '';
    }
}

async function sendFriendRequest(force) {
    const peerId = document.getElementById('friend-peer-id').value.trim();
    const message = document.getElementById('friend-message').value.trim();

    if (!peerId) {
        showToast('Peer ID is required');
        return;
    }

    try {
        const payload = { peer_id: peerId };
        if (message) payload.message = message;
        if (force) payload.force = true;

        await apiPost('/api/friend-requests', payload);
        showToast('Friend request sent');
        toggleAddFriendForm();
        await loadFriendRequests();
    } catch (e) {
        if (e.message && e.message.includes('already pending')) {
            if (confirm('A friend request to this peer is already pending. Resend it?')) {
                await sendFriendRequest(true);
            }
        } else {
            showToast('Failed to send request: ' + e.message);
        }
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

        return '<div class="fr-item" data-status="' + r.status + '" data-fr-from="' + escapeHtml(otherPeerId) + '">'
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
        // Update last_seen_online for the sender in our local peers list
        if (event.sender_id !== myPeerId) {
            const senderPeer = peers.find(p => p.peer_id === event.sender_id);
            if (senderPeer) {
                const msgTs = event.timestamp || Math.floor(Date.now() / 1000);
                if (!senderPeer.last_seen_online || msgTs > senderPeer.last_seen_online) {
                    senderPeer.last_seen_online = msgTs;
                    renderFriendsList();
                }
            }
        }

        const msg = {
            message_id: event.message_id,
            sender_id: event.sender_id,
            message_kind: event.message_kind,
            body: event.body,
            timestamp: event.timestamp,
            is_read: event.sender_id === myPeerId,
            reply_to: event.reply_to || null,
        };

        // Only show on timeline if it's a root public/group post
        const isTimelinePost = (event.message_kind === 'public' || event.message_kind === 'friend_group') && !msg.reply_to;
        const matchesFilter = !currentFilter || event.message_kind === currentFilter;

        if (isTimelinePost && matchesFilter && currentView === 'timeline') {
            // Avoid duplicates
            if (!messages.find(m => m.message_id === msg.message_id)) {
                messages.unshift(msg);
                renderTimeline();
            }
        }

        if (event.sender_id !== myPeerId) {
            const senderPeer = peers.find(p => p.peer_id === event.sender_id);
            const senderName = senderPeer?.display_name || event.sender_id.substring(0, 12) + '...';
            showToast('New message from ' + senderName);
        }

        // If it's a direct message, refresh conversation detail if viewing that peer
        if (event.message_kind === 'direct') {
            if (currentView === 'conversation-detail') {
                const otherPeerId = event.sender_id === myPeerId ? msg.recipient_id : event.sender_id;
                if (otherPeerId === currentConversationPeerId) {
                    loadConversationMessages(currentConversationPeerId, false);
                }
            }
        }
    } else if (event.type === 'message_read') {
        const el = document.querySelector(`.message[data-id="${event.message_id}"], .dm-message[data-id="${event.message_id}"]`);
        if (el) el.classList.remove('unread');
    } else if (event.type === 'peer_online') {
        // Update peer online status
        const peer = peers.find(p => p.peer_id === event.peer_id);
        if (peer) {
            peer.online = true;
            peer.last_seen_online = Math.floor(Date.now() / 1000);
            renderFriendsList();
            // Update DM peer info if viewing that peer
            if (currentView === 'conversation-detail' && currentConversationPeerId === event.peer_id) {
                renderDmPeerInfo(event.peer_id);
            }
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
            // Update DM peer info if viewing that peer
            if (currentView === 'conversation-detail' && currentConversationPeerId === event.peer_id) {
                renderDmPeerInfo(event.peer_id);
            }
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
    } else if (event.type === 'profile_updated') {
        const peer = peers.find(p => p.peer_id === event.peer_id);
        if (peer) {
            if (event.display_name !== undefined) peer.display_name = event.display_name;
            renderFriendsList();
            if (currentView === 'conversation-detail' && currentConversationPeerId === event.peer_id) {
                renderDmPeerInfo(event.peer_id);
            }
        }
    } else if (event.type === 'notification') {
        // New notification received (Phase 11)
        // Skip if the user is already viewing the relevant DM conversation
        const activelyViewingDm = event.notification_type === 'direct_message'
            && currentView === 'conversation-detail'
            && currentConversationPeerId === event.sender_id;
        if (!activelyViewingDm) {
            if (!notificationPanelOpen) {
                unseenNotificationCount++;
                updateNotificationBadge();
            } else {
                const newNotif = {
                    id: event.id,
                    type: event.notification_type,
                    message_id: event.message_id,
                    sender_id: event.sender_id,
                    created_at: event.created_at,
                    read: false,
                };
                notifications.unshift(newNotif);
                renderNotificationList();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Comments (Phase 9)
// ---------------------------------------------------------------------------
async function loadReplies(messageId, append) {
    let url = `/api/messages/${encodeURIComponent(messageId)}/replies?limit=${COMMENTS_PAGE_SIZE}`;
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
        console.error('Failed to load comments:', e);
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
    loadMoreEl.style.display = currentPostReplies.length >= COMMENTS_PAGE_SIZE ? '' : 'none';
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
            <span class="msg-time" title="${msgTimeTitle(m)}">${timeAgo(m.timestamp)}</span>
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
        showToast('Comment sent');
        // Reload comments
        await loadReplies(currentPostId, false);
    } catch (e) {
        showToast('Comment failed: ' + e.message);
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

function setActiveHeaderNav(activeId) {
    ['nav-feed', 'nav-groups', 'nav-friends', 'nav-profile'].forEach(id => {
        const el = document.getElementById(id);
        if (el) el.classList.toggle('active', id === activeId);
    });
}

function navToFeed() {
    document.querySelectorAll('.filters button').forEach(b => b.classList.remove('active'));
    document.querySelector('.filters button[data-kind="public"]').classList.add('active');
    currentFilter = 'public';
    oldestTimestamp = null;
    showTimelineView();
    loadMessages(false);
    setActiveHeaderNav('nav-feed');
}

function navToGroups() {
    document.querySelectorAll('.filters button').forEach(b => b.classList.remove('active'));
    document.querySelector('.filters button[data-kind="friend_group"]').classList.add('active');
    currentFilter = 'friend_group';
    oldestTimestamp = null;
    showTimelineView();
    loadMessages(false);
    setActiveHeaderNav('nav-groups');
}

function navToFriends() {
    navigateTo('friends');
}

async function showFriendsView(fromRoute) {
    currentView = 'friends';
    hideAllViews();
    document.querySelector('.filters').style.display = 'none';
    document.getElementById('compose-box').style.display = 'none';
    document.getElementById('friends-view').classList.add('visible');
    setActiveHeaderNav('nav-friends');

    if (!fromRoute) {
        window.location.hash = '#/friends';
    }

    await loadFriendRequests();

    if (pendingFriendRequestHighlight) {
        const hlId = pendingFriendRequestHighlight;
        pendingFriendRequestHighlight = null;
        highlightFriendRequest(hlId);
    }
}

function highlightFriendRequest(peerId) {
    // Ensure pending tab is active so the request is visible
    switchFrTab('pending');
    const item = document.querySelector('.fr-item[data-fr-from="' + peerId + '"]');
    if (item) {
        item.scrollIntoView({ behavior: 'smooth', block: 'center' });
        item.classList.add('fr-item-highlight');
        setTimeout(() => item.classList.remove('fr-item-highlight'), 2500);
    }
}

function navToProfile() {
    showMyProfile();
}

function showMyProfile(fromRoute) {
    previousView = currentView;
    currentView = 'profile-edit';
    viewingProfileId = myPeerId;

    hideAllViews();
    document.querySelector('.filters').style.display = 'none';
    document.getElementById('profile-edit').classList.add('visible');
    document.getElementById('compose-box').style.display = 'none';
    setActiveHeaderNav('nav-profile');

    if (!fromRoute) {
        window.location.hash = '#/profile';
    }

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
    document.querySelector('.filters').style.display = 'none';
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

    // Add button to open DM conversation if they are a peer
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

    // Restore previous view based on current hash
    const hash = window.location.hash || '#/';
    const parts = hash.replace('#/', '').split('/');
    const route = parts[0] || '';

    if (route === 'peer' && parts[1]) {
        showConversationDetail(decodeURIComponent(parts.slice(1).join('/')), true);
    } else if (route === 'post' && parts[1]) {
        showPostDetail(decodeURIComponent(parts.slice(1).join('/')), true);
    } else if (route === 'friends') {
        showFriendsView(true);
    } else {
        showTimelineView();
        loadMessages(false);
        setActiveHeaderNav(currentFilter === 'friend_group' ? 'nav-groups' : 'nav-feed');
    }
}

// ---------------------------------------------------------------------------
// Notifications (Phase 11)
// ---------------------------------------------------------------------------
async function loadNotifications() {
    try {
        notifications = await apiGet('/api/notifications?limit=50');
        renderNotificationList();
    } catch (e) {
        console.error('Failed to load notifications:', e);
    }
}

async function loadNotificationCount() {
    try {
        const data = await apiGet('/api/notifications/count');
        unseenNotificationCount = data.unseen || 0;
        updateNotificationBadge();
    } catch (e) {
        console.error('Failed to load notification count:', e);
    }
}

function updateNotificationBadge() {
    const badge = document.getElementById('notification-badge');
    if (unseenNotificationCount > 0) {
        badge.textContent = unseenNotificationCount > 99 ? '99+' : unseenNotificationCount;
        badge.style.display = '';
    } else {
        badge.style.display = 'none';
    }
}

function toggleNotificationPanel() {
    notificationPanelOpen = !notificationPanelOpen;
    const panel = document.getElementById('notification-panel');
    if (notificationPanelOpen) {
        panel.classList.add('visible');
        unseenNotificationCount = 0;
        updateNotificationBadge();
        loadNotifications();
        apiPost('/api/notifications/seen-all', {}).catch(() => {});
    } else {
        panel.classList.remove('visible');
    }
}

function renderNotificationList() {
    const el = document.getElementById('notification-list');
    if (notifications.length === 0) {
        el.innerHTML = '<div class="notification-empty">No notifications</div>';
        return;
    }
    el.innerHTML = notifications.map(renderNotificationItem).join('');
}

function renderNotificationItem(n) {
    const unreadClass = n.read ? '' : ' unread';
    const typeLabel = getNotificationTypeLabel(n.type);
    const senderShort = n.sender_id ? n.sender_id.substring(0, 12) + '...' : 'Someone';
    const peer = peers.find(p => p.peer_id === n.sender_id);
    const senderName = peer?.display_name || senderShort;

    let message = '';
    switch (n.type) {
        case 'direct_message':
            message = `${senderName} sent you a message`;
            break;
        case 'reply':
            message = `${senderName} commented on your post`;
            break;
        case 'reaction':
            message = `${senderName} reacted to your post`;
            break;
        case 'friend_request':
            message = `${senderName} sent a friend request`;
            break;
        default:
            message = `${senderName} - ${typeLabel}`;
    }

    return `<div class="notification-item${unreadClass}" data-id="${n.id}" onclick="handleNotificationClick(${n.id}, '${n.type}', '${n.message_id}', '${n.sender_id}')">
        <div class="notification-content">
            <span class="notification-type-icon">${getNotificationIcon(n.type)}</span>
            <div class="notification-text">
                <div class="notification-message">${escapeHtml(message)}</div>
                <div class="notification-time" title="${exactTime(n.created_at)}">${timeAgo(n.created_at)}</div>
            </div>
        </div>
    </div>`;
}

function getNotificationTypeLabel(type) {
    switch (type) {
        case 'direct_message': return 'Message';
        case 'reply': return 'Comment';
        case 'reaction': return 'Reaction';
        case 'friend_request': return 'Friend Request';
        default: return 'Notification';
    }
}

function getNotificationIcon(type) {
    switch (type) {
        case 'direct_message': return '&#128172;'; // speech bubble
        case 'reply': return '&#128172;'; // speech bubble
        case 'reaction': return '&#9650;'; // upvote arrow
        case 'friend_request': return '&#128100;'; // person
        default: return '&#128276;'; // bell
    }
}

async function handleNotificationClick(id, type, messageId, senderId) {
    // Mark as read
    try {
        await apiPost(`/api/notifications/${id}/read`, {});
        const notif = notifications.find(n => n.id === id);
        if (notif) notif.read = true;
        renderNotificationList();
    } catch (e) {
        console.error('Failed to mark notification read:', e);
    }

    // Navigate based on type
    toggleNotificationPanel();
    if (type === 'direct_message') {
        navigateTo('peer/' + encodeURIComponent(senderId));
    } else if (type === 'reply' || type === 'reaction') {
        if (messageId) {
            navigateTo('post/' + encodeURIComponent(messageId));
        }
    } else if (type === 'friend_request') {
        pendingFriendRequestHighlight = senderId;
        navigateTo('friends');
    }
}

async function markAllNotificationsRead() {
    try {
        await apiPost('/api/notifications/read-all', {});
        notifications.forEach(n => n.read = true);
        renderNotificationList();
        unseenNotificationCount = 0;
        updateNotificationBadge();
        showToast('All notifications marked as read');
    } catch (e) {
        showToast('Failed to mark notifications read');
    }
}

// Close notification panel when clicking outside
document.addEventListener('click', (e) => {
    const panel = document.getElementById('notification-panel');
    const bell = document.getElementById('notification-bell');
    if (notificationPanelOpen && !panel.contains(e.target) && !bell.contains(e.target)) {
        notificationPanelOpen = false;
        panel.classList.remove('visible');
    }
});

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
    await loadNotificationCount(); // Phase 11

    // Route based on current hash
    handleRoute();

    connectWs();
}

init();
