package com.example.tenet.ui.groups

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiGroup
import com.example.tenet.uniffi.FfiGroupMember
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class GroupDetailUiState(
    val group: FfiGroup? = null,
    val members: List<FfiGroupMember> = emptyList(),
    val isLoading: Boolean = false,
    val error: String? = null,
    val actionSuccess: String? = null,
    val leftGroup: Boolean = false,
)

@HiltViewModel
class GroupDetailViewModel @Inject constructor(
    savedStateHandle: SavedStateHandle,
    private val repository: TenetRepository,
) : ViewModel() {

    private val groupId: String = checkNotNull(savedStateHandle["groupId"])

    private val _uiState = MutableStateFlow(GroupDetailUiState())
    val uiState: StateFlow<GroupDetailUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            runCatching {
                val group = repository.getGroup(groupId)
                val members = repository.listGroupMembers(groupId)
                Pair(group, members)
            }.onSuccess { (group, members) ->
                _uiState.value = _uiState.value.copy(
                    group = group,
                    members = members,
                    isLoading = false,
                )
            }.onFailure { e ->
                _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
            }
        }
    }

    fun addMember(peerId: String) {
        viewModelScope.launch {
            runCatching { repository.addGroupMember(groupId, peerId) }
                .onSuccess {
                    _uiState.value = _uiState.value.copy(actionSuccess = "Member added")
                    load()
                }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun removeMember(peerId: String) {
        viewModelScope.launch {
            runCatching { repository.removeGroupMember(groupId, peerId) }
                .onSuccess { load() }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun leaveGroup() {
        viewModelScope.launch {
            runCatching { repository.leaveGroup(groupId) }
                .onSuccess { _uiState.value = _uiState.value.copy(leftGroup = true) }
                .onFailure { e -> _uiState.value = _uiState.value.copy(error = e.message) }
        }
    }

    fun clearStatus() {
        _uiState.value = _uiState.value.copy(error = null, actionSuccess = null)
    }
}
