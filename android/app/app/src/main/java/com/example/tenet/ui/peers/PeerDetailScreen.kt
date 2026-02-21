package com.example.tenet.ui.peers

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.hilt.navigation.compose.hiltViewModel

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PeerDetailScreen(
    onBack: () -> Unit,
    onSendMessage: (String) -> Unit,
    viewModel: PeerDetailViewModel = hiltViewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val snackbarHostState = remember { SnackbarHostState() }
    var showFriendRequestDialog by remember { mutableStateOf(false) }

    LaunchedEffect(state.error) {
        state.error?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }
    LaunchedEffect(state.actionSuccess) {
        state.actionSuccess?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }

    val peer = state.peer
    val profile = state.profile

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(peer?.displayName ?: peer?.peerId?.take(16) ?: "Peer") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        if (state.isLoading) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding),
            ) {
                CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
            }
            return@Scaffold
        }

        if (peer == null) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding),
            ) {
                Text("Peer not found", modifier = Modifier.align(Alignment.Center))
            }
            return@Scaffold
        }

        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            // --- Identity ---
            Text("Peer ID", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
            Text(peer.peerId, style = MaterialTheme.typography.bodyMedium)

            if (profile != null) {
                HorizontalDivider()
                if (profile.bio != null) {
                    Text("Bio", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
                    Text(profile.bio, style = MaterialTheme.typography.bodyMedium)
                }
            }

            HorizontalDivider()

            // --- Status badges ---
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                StatusChip(
                    label = if (peer.isOnline) "Online" else "Offline",
                    active = peer.isOnline,
                )
                if (peer.isFriend) StatusChip("Friend", active = true)
                if (peer.isBlocked) StatusChip("Blocked", active = true, isError = true)
                if (peer.isMuted) StatusChip("Muted", active = true)
            }

            HorizontalDivider()
            Spacer(Modifier.height(4.dp))

            // --- Actions ---
            Button(
                onClick = { onSendMessage(peer.peerId) },
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Send Message") }

            if (!peer.isFriend && state.pendingOutgoingRequest == null) {
                OutlinedButton(
                    onClick = { showFriendRequestDialog = true },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Send Friend Request") }
            } else if (state.pendingOutgoingRequest != null) {
                OutlinedButton(
                    onClick = {},
                    enabled = false,
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Friend Request Pending") }
            }

            if (peer.isBlocked) {
                OutlinedButton(
                    onClick = viewModel::unblock,
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Unblock") }
            } else {
                OutlinedButton(
                    onClick = viewModel::block,
                    modifier = Modifier.fillMaxWidth(),
                    colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
                ) { Text("Block") }
            }

            if (peer.isMuted) {
                OutlinedButton(
                    onClick = viewModel::unmute,
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Unmute") }
            } else {
                OutlinedButton(
                    onClick = viewModel::mute,
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Mute") }
            }

            Spacer(Modifier.height(8.dp))

            TextButton(
                onClick = { viewModel.remove(); onBack() },
                colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error),
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Remove Peer") }
        }
    }

    if (showFriendRequestDialog) {
        FriendRequestDialog(
            onDismiss = { showFriendRequestDialog = false },
            onConfirm = { msg ->
                showFriendRequestDialog = false
                viewModel.sendFriendRequest(msg)
            },
        )
    }
}

@Composable
private fun StatusChip(label: String, active: Boolean, isError: Boolean = false) {
    val color = when {
        isError -> MaterialTheme.colorScheme.error
        active -> MaterialTheme.colorScheme.primary
        else -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Text(
        text = label,
        style = MaterialTheme.typography.labelMedium,
        fontWeight = FontWeight.SemiBold,
        color = color,
    )
}

@Composable
private fun FriendRequestDialog(onDismiss: () -> Unit, onConfirm: (String?) -> Unit) {
    var message by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Send Friend Request") },
        text = {
            OutlinedTextField(
                value = message,
                onValueChange = { message = it },
                label = { Text("Message (optional)") },
                modifier = Modifier.fillMaxWidth(),
                maxLines = 3,
            )
        },
        confirmButton = {
            Button(onClick = { onConfirm(message.ifBlank { null }) }) { Text("Send") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
