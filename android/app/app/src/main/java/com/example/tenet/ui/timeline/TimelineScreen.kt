package com.example.tenet.ui.timeline

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
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
import com.example.tenet.uniffi.FfiMessage
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

/**
 * Public timeline screen (Phase 1 — read-only).
 *
 * Displays fetched public messages sorted newest-first.  A FAB triggers a
 * manual relay sync.  Future phases will add a compose FAB, tabs
 * (Public / Friend Group), and pull-to-refresh.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun TimelineScreen(viewModel: TimelineViewModel) {
    val state by viewModel.uiState.collectAsState()

    Scaffold(
        topBar = {
            TopAppBar(title = { Text("Timeline") })
        },
        floatingActionButton = {
            FloatingActionButton(onClick = { viewModel.sync() }) {
                Icon(Icons.Default.Refresh, contentDescription = "Sync")
            }
        }
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding)
        ) {
            when {
                state.isLoading -> {
                    CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
                }

                state.error != null -> {
                    Text(
                        text = "Error: ${state.error}",
                        modifier = Modifier
                            .align(Alignment.Center)
                            .padding(16.dp),
                        color = MaterialTheme.colorScheme.error,
                    )
                }

                state.messages.isEmpty() -> {
                    Text(
                        text = "No messages yet. Tap sync to fetch from relay.",
                        modifier = Modifier
                            .align(Alignment.Center)
                            .padding(16.dp),
                    )
                }

                else -> {
                    LazyColumn(
                        modifier = Modifier.fillMaxSize(),
                        contentPadding = PaddingValues(8.dp),
                    ) {
                        items(state.messages, key = { it.messageId }) { message ->
                            MessageCard(message = message)
                        }
                    }
                }
            }

            if (state.isSyncing) {
                CircularProgressIndicator(modifier = Modifier.align(Alignment.TopEnd).padding(16.dp))
            }
        }
    }
}

@Composable
private fun MessageCard(message: FfiMessage) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 4.dp)
    ) {
        Column(modifier = Modifier.padding(12.dp)) {
            Text(
                text = message.senderId.take(16),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.primary,
            )
            Text(
                text = message.body,
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.padding(top = 4.dp),
            )
            Text(
                text = formatTimestamp(message.timestamp),
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
    }
}

private val TIME_FMT = DateTimeFormatter.ofPattern("MMM d, HH:mm")
    .withZone(ZoneId.systemDefault())

private fun formatTimestamp(epochSecs: Long): String =
    TIME_FMT.format(Instant.ofEpochSecond(epochSecs))
