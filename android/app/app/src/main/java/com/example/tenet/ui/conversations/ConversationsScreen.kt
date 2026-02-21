package com.example.tenet.ui.conversations

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Badge
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.hilt.navigation.compose.hiltViewModel
import com.example.tenet.uniffi.FfiConversation
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

/**
 * DM conversation list screen.
 *
 * Shows all direct-message conversations grouped by peer, sorted by most
 * recent.  Tapping a row navigates to [ConversationDetailScreen].
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConversationsScreen(
    onConversationClick: (String) -> Unit,
    viewModel: ConversationsViewModel = hiltViewModel(),
) {
    val state by viewModel.uiState.collectAsState()

    Scaffold(
        topBar = { TopAppBar(title = { Text("Messages") }) },
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding),
        ) {
            when {
                state.isLoading -> CircularProgressIndicator(
                    modifier = Modifier.align(Alignment.Center),
                )

                state.error != null -> Text(
                    text = "Error: ${state.error}",
                    modifier = Modifier
                        .align(Alignment.Center)
                        .padding(16.dp),
                    color = MaterialTheme.colorScheme.error,
                )

                state.conversations.isEmpty() -> Text(
                    text = "No conversations yet.",
                    modifier = Modifier
                        .align(Alignment.Center)
                        .padding(16.dp),
                )

                else -> LazyColumn(
                    modifier = Modifier.fillMaxSize(),
                    contentPadding = PaddingValues(8.dp),
                ) {
                    items(state.conversations, key = { it.peerId }) { convo ->
                        ConversationRow(
                            convo = convo,
                            onClick = { onConversationClick(convo.peerId) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun ConversationRow(convo: FfiConversation, onClick: () -> Unit) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 4.dp)
            .clickable(onClick = onClick),
    ) {
        Row(
            modifier = Modifier.padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = convo.displayName ?: convo.peerId.take(16),
                    style = MaterialTheme.typography.titleSmall,
                )
                if (convo.lastMessage != null) {
                    Text(
                        text = convo.lastMessage!!,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                }
            }

            Column(
                horizontalAlignment = Alignment.End,
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Text(
                    text = formatTs(convo.lastTimestamp),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                if (convo.unreadCount > 0u) {
                    Badge { Text(text = "${convo.unreadCount}") }
                }
            }
        }
    }
}

private val TS_FMT = DateTimeFormatter.ofPattern("MMM d").withZone(ZoneId.systemDefault())

private fun formatTs(epochSecs: Long): String = TS_FMT.format(Instant.ofEpochSecond(epochSecs))
