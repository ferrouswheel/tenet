package com.example.tenet.ui.friends

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Group
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.hilt.navigation.compose.hiltViewModel
import com.example.tenet.uniffi.FfiFriendRequest

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun FriendsScreen(
    onPeerClick: (String) -> Unit,
    onGroupsClick: () -> Unit = {},
    viewModel: FriendsViewModel = hiltViewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val snackbarHostState = remember { SnackbarHostState() }

    LaunchedEffect(state.error) {
        state.error?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }
    LaunchedEffect(state.actionSuccess) {
        state.actionSuccess?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Friends") },
                actions = {
                    IconButton(onClick = onGroupsClick) {
                        Icon(Icons.Default.Group, contentDescription = "Groups")
                    }
                },
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            if (state.isLoading) {
                CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
                return@Box
            }

            if (state.incoming.isEmpty() && state.outgoing.isEmpty()) {
                Text(
                    "No friend requests yet. Go to a peer's profile to send one.",
                    modifier = Modifier
                        .align(Alignment.Center)
                        .padding(24.dp),
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                return@Box
            }

            LazyColumn(modifier = Modifier.fillMaxSize()) {
                if (state.incoming.isNotEmpty()) {
                    item {
                        SectionHeader("Incoming Requests (${state.incoming.size})")
                    }
                    items(state.incoming, key = { it.id }) { req ->
                        IncomingRequestCard(
                            request = req,
                            onAccept = { viewModel.accept(req.id) },
                            onIgnore = { viewModel.ignore(req.id) },
                            onBlock = { viewModel.block(req.id) },
                            onPeerClick = onPeerClick,
                        )
                    }
                }

                if (state.outgoing.isNotEmpty()) {
                    item {
                        HorizontalDivider(modifier = Modifier.padding(vertical = 8.dp))
                        SectionHeader("Outgoing Requests (${state.outgoing.size})")
                    }
                    items(state.outgoing, key = { it.id }) { req ->
                        OutgoingRequestCard(request = req, onPeerClick = onPeerClick)
                    }
                }
            }
        }
    }
}

@Composable
private fun SectionHeader(title: String) {
    Text(
        text = title,
        style = MaterialTheme.typography.titleSmall,
        fontWeight = FontWeight.SemiBold,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
    )
}

@Composable
private fun IncomingRequestCard(
    request: FfiFriendRequest,
    onAccept: () -> Unit,
    onIgnore: () -> Unit,
    onBlock: () -> Unit,
    onPeerClick: (String) -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 6.dp),
    ) {
        Column(modifier = Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(
                text = request.fromPeerId.take(20),
                style = MaterialTheme.typography.bodyMedium,
                fontWeight = FontWeight.Medium,
            )
            if (!request.message.isNullOrBlank()) {
                Text(
                    text = "\"${request.message}\"",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = onAccept) { Text("Accept") }
                OutlinedButton(onClick = onIgnore) { Text("Ignore") }
                OutlinedButton(
                    onClick = onBlock,
                    colors = ButtonDefaults.outlinedButtonColors(
                        contentColor = MaterialTheme.colorScheme.error,
                    ),
                ) { Text("Block") }
            }
        }
    }
}

@Composable
private fun OutgoingRequestCard(request: FfiFriendRequest, onPeerClick: (String) -> Unit) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp, vertical = 6.dp),
        onClick = { onPeerClick(request.toPeerId) },
    ) {
        Row(
            modifier = Modifier.padding(12.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = request.toPeerId.take(20),
                style = MaterialTheme.typography.bodyMedium,
                fontWeight = FontWeight.Medium,
                modifier = Modifier.weight(1f),
            )
            Text(
                text = request.status.replaceFirstChar { it.uppercase() },
                style = MaterialTheme.typography.labelSmall,
                color = when (request.status) {
                    "accepted" -> MaterialTheme.colorScheme.primary
                    "ignored", "blocked" -> MaterialTheme.colorScheme.error
                    else -> MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
        }
    }
}
