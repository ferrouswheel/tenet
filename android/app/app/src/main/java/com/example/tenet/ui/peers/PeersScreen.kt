package com.example.tenet.ui.peers

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Circle
import androidx.compose.material.icons.filled.QrCodeScanner
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
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
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.hilt.navigation.compose.hiltViewModel
import com.example.tenet.uniffi.FfiPeer
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions

/**
 * Peers screen — lists all known peers and provides a floating action button to
 * add a new peer by ID / signing key or by scanning a QR code.
 *
 * Phase 4 addition: the "Add Peer" dialog now includes a QR scan icon button
 * (using ZXing Android Embedded's [ScanContract]) that pre-fills the peer ID
 * field when a valid QR code is scanned.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PeersScreen(
    onPeerClick: (String) -> Unit,
    viewModel: PeersViewModel = hiltViewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val snackbarHostState = remember { SnackbarHostState() }

    LaunchedEffect(state.error) {
        state.error?.let {
            snackbarHostState.showSnackbar(it)
            viewModel.clearError()
        }
    }

    Scaffold(
        topBar = { TopAppBar(title = { Text("Peers") }) },
        floatingActionButton = {
            FloatingActionButton(onClick = viewModel::showAddPeerDialog) {
                Icon(Icons.Default.Add, contentDescription = "Add peer")
            }
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { padding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            if (state.isLoading && state.peers.isEmpty()) {
                CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
            } else if (state.peers.isEmpty()) {
                Text(
                    "No peers yet. Tap + to add one.",
                    modifier = Modifier
                        .align(Alignment.Center)
                        .padding(24.dp),
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                LazyColumn {
                    items(state.peers, key = { it.peerId }) { peer ->
                        PeerRow(peer = peer, onClick = { onPeerClick(peer.peerId) })
                    }
                }
            }
        }
    }

    if (state.showAddDialog) {
        AddPeerDialog(
            onDismiss = viewModel::hideAddPeerDialog,
            onConfirm = { peerId, displayName, key -> viewModel.addPeer(peerId, displayName, key) },
        )
    }
}

@Composable
private fun PeerRow(peer: FfiPeer, onClick: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            imageVector = Icons.Default.Circle,
            contentDescription = if (peer.isOnline) "Online" else "Offline",
            tint = if (peer.isOnline) MaterialTheme.colorScheme.primary
            else MaterialTheme.colorScheme.outlineVariant,
            modifier = Modifier.size(10.dp),
        )
        Spacer(Modifier.width(12.dp))
        Column(modifier = Modifier.weight(1f)) {
            Text(
                text = peer.displayName ?: peer.peerId.take(16),
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = FontWeight.Medium,
            )
            if (peer.displayName != null) {
                Text(
                    text = peer.peerId.take(20),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            if (peer.isFriend) BadgeLabel("Friend")
            if (peer.isBlocked) BadgeLabel("Blocked", error = true)
            if (peer.isMuted) BadgeLabel("Muted")
        }
    }
}

@Composable
private fun BadgeLabel(label: String, error: Boolean = false) {
    Text(
        text = label,
        style = MaterialTheme.typography.labelSmall,
        color = if (error) MaterialTheme.colorScheme.error
        else MaterialTheme.colorScheme.primary,
    )
}

/**
 * "Add Peer" dialog with manual entry fields and a QR-scan shortcut.
 *
 * Tapping the QR icon button launches the ZXing scanner activity.  On a
 * successful scan the peer ID field is pre-filled with the scanned string.
 * The signing key must still be entered manually (the Tenet QR code encodes
 * only the peer ID string for now).
 */
@Composable
private fun AddPeerDialog(
    onDismiss: () -> Unit,
    onConfirm: (peerId: String, displayName: String?, signingKey: String) -> Unit,
) {
    var peerId by remember { mutableStateOf("") }
    var displayName by remember { mutableStateOf("") }
    var signingKey by remember { mutableStateOf("") }

    // ZXing Android Embedded scanner launcher — fills the peer ID field on scan.
    val scanLauncher = rememberLauncherForActivityResult(ScanContract()) { result ->
        result.contents?.let { scanned -> peerId = scanned.trim() }
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add Peer") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                // Peer ID field with QR scan icon on the right.
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    OutlinedTextField(
                        value = peerId,
                        onValueChange = { peerId = it },
                        label = { Text("Peer ID") },
                        singleLine = true,
                        modifier = Modifier.weight(1f),
                    )
                    Spacer(Modifier.width(4.dp))
                    IconButton(
                        onClick = {
                            scanLauncher.launch(
                                ScanOptions().apply {
                                    setDesiredBarcodeFormats(ScanOptions.QR_CODE)
                                    setPrompt("Scan peer QR code")
                                    setBeepEnabled(true)
                                    setOrientationLocked(false)
                                }
                            )
                        },
                    ) {
                        Icon(
                            Icons.Default.QrCodeScanner,
                            contentDescription = "Scan QR code",
                            tint = MaterialTheme.colorScheme.primary,
                        )
                    }
                }
                OutlinedTextField(
                    value = displayName,
                    onValueChange = { displayName = it },
                    label = { Text("Display name (optional)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = signingKey,
                    onValueChange = { signingKey = it },
                    label = { Text("Signing public key (hex)") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Ascii),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(4.dp))
            }
        },
        confirmButton = {
            Button(
                onClick = { onConfirm(peerId.trim(), displayName.ifBlank { null }, signingKey.trim()) },
                enabled = peerId.isNotBlank() && signingKey.isNotBlank(),
            ) { Text("Add") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
