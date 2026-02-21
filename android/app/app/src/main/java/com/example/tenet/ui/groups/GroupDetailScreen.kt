package com.example.tenet.ui.groups

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
import com.example.tenet.uniffi.FfiGroupMember

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun GroupDetailScreen(
    onBack: () -> Unit,
    viewModel: GroupDetailViewModel = hiltViewModel(),
) {
    val state by viewModel.uiState.collectAsState()
    val snackbarHostState = remember { SnackbarHostState() }
    var showAddMemberDialog by remember { mutableStateOf(false) }
    var showLeaveConfirm by remember { mutableStateOf(false) }

    LaunchedEffect(state.error) {
        state.error?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }
    LaunchedEffect(state.actionSuccess) {
        state.actionSuccess?.let { snackbarHostState.showSnackbar(it); viewModel.clearStatus() }
    }
    LaunchedEffect(state.leftGroup) {
        if (state.leftGroup) onBack()
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("Group") },
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
            Box(Modifier.fillMaxSize().padding(padding)) {
                CircularProgressIndicator(modifier = Modifier.align(Alignment.Center))
            }
            return@Scaffold
        }

        val group = state.group
        if (group == null) {
            Box(Modifier.fillMaxSize().padding(padding)) {
                Text("Group not found", modifier = Modifier.align(Alignment.Center))
            }
            return@Scaffold
        }

        LazyColumn(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            item {
                Column(modifier = Modifier.padding(16.dp)) {
                    Text("Group ID", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
                    Text(group.groupId, style = MaterialTheme.typography.bodyMedium)
                    Text(
                        "Created by ${group.creatorId.take(16)}",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                    HorizontalDivider(modifier = Modifier.padding(vertical = 12.dp))
                    Text(
                        "Members (${state.members.size})",
                        style = MaterialTheme.typography.titleSmall,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
            }

            items(state.members, key = { it.peerId }) { member ->
                MemberRow(
                    member = member,
                    onRemove = { viewModel.removeMember(member.peerId) },
                )
            }

            item {
                HorizontalDivider(modifier = Modifier.padding(16.dp))
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    OutlinedButton(
                        onClick = { showAddMemberDialog = true },
                        modifier = Modifier.fillMaxWidth(),
                    ) { Text("Add Member") }
                    OutlinedButton(
                        onClick = { showLeaveConfirm = true },
                        modifier = Modifier.fillMaxWidth(),
                        colors = ButtonDefaults.outlinedButtonColors(
                            contentColor = MaterialTheme.colorScheme.error,
                        ),
                    ) { Text("Leave Group") }
                }
            }
        }
    }

    if (showAddMemberDialog) {
        AddMemberDialog(
            onDismiss = { showAddMemberDialog = false },
            onConfirm = { peerId ->
                showAddMemberDialog = false
                viewModel.addMember(peerId)
            },
        )
    }

    if (showLeaveConfirm) {
        AlertDialog(
            onDismissRequest = { showLeaveConfirm = false },
            title = { Text("Leave Group") },
            text = { Text("Are you sure you want to leave this group?") },
            confirmButton = {
                Button(
                    onClick = { showLeaveConfirm = false; viewModel.leaveGroup() },
                    colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error),
                ) { Text("Leave") }
            },
            dismissButton = { TextButton(onClick = { showLeaveConfirm = false }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun MemberRow(member: FfiGroupMember, onRemove: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(
                text = member.displayName ?: member.peerId.take(20),
                style = MaterialTheme.typography.bodyMedium,
                fontWeight = FontWeight.Medium,
            )
            if (member.displayName != null) {
                Text(
                    text = member.peerId.take(16),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        TextButton(
            onClick = onRemove,
            colors = ButtonDefaults.textButtonColors(contentColor = MaterialTheme.colorScheme.error),
        ) { Text("Remove") }
    }
}

@Composable
private fun AddMemberDialog(onDismiss: () -> Unit, onConfirm: (String) -> Unit) {
    var peerId by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add Member") },
        text = {
            OutlinedTextField(
                value = peerId,
                onValueChange = { peerId = it },
                label = { Text("Peer ID") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
        },
        confirmButton = {
            Button(
                onClick = { onConfirm(peerId.trim()) },
                enabled = peerId.isNotBlank(),
            ) { Text("Add") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
