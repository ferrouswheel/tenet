package com.example.tenet.ui.groups

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiGroup
import com.example.tenet.uniffi.FfiGroupInvite
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class GroupsUiState(
    val groups: List<FfiGroup> = emptyList(),
    val pendingInvites: List<FfiGroupInvite> = emptyList(),
    val isLoading: Boolean = false,
    val error: String? = null,
    val actionSuccess: String? = null,
    val showCreateDialog: Boolean = false,
)

@HiltViewModel
class GroupsViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(GroupsUiState())
    val uiState: StateFlow<GroupsUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            runCatching {
                val groups = repository.listGroups()
                val invites = repository.listGroupInvites()
                    .filter { it.direction == "incoming" && it.status == "pending" }
                Pair(groups, invites)
            }.onSuccess { (groups, invites) ->
                _uiState.value = _uiState.value.copy(
                    groups = groups,
                    pendingInvites = invites,
                    isLoading = false,
                )
            }.onFailure { e ->
                _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
            }
        }
    }

    fun showCreateDialog() {
        _uiState.value = _uiState.value.copy(showCreateDialog = true)
    }

    fun hideCreateDialog() {
        _uiState.value = _uiState.value.copy(showCreateDialog = false)
    }

    /** memberIds is a comma-separated string of peer IDs entered by the user. */
    fun createGroup(memberIdsRaw: String) {
        val memberIds = memberIdsRaw.split(",").map { it.trim() }.filter { it.isNotBlank() }
        viewModelScope.launch {
            runCatching { repository.createGroup(memberIds) }
                .onSuccess { groupId ->
                    _uiState.value = _uiState.value.copy(
                        showCreateDialog = false,
                        actionSuccess = "Group created: $groupId",
                    )
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun acceptInvite(inviteId: Long) {
        viewModelScope.launch {
            runCatching { repository.acceptGroupInvite(inviteId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Joined group")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun ignoreInvite(inviteId: Long) {
        viewModelScope.launch {
            runCatching { repository.ignoreGroupInvite(inviteId) }
                .onSuccess { load() }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun clearStatus() {
        _uiState.value = _uiState.value.copy(error = null, actionSuccess = null)
    }
}
