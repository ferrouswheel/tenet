package com.example.tenet.ui.timeline

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiMessage
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

enum class TimelineTab { PUBLIC, FRIEND_GROUP }

data class TimelineUiState(
    val messages: List<FfiMessage> = emptyList(),
    val isLoading: Boolean = false,
    val isSyncing: Boolean = false,
    val error: String? = null,
    val activeTab: TimelineTab = TimelineTab.PUBLIC,
)

@HiltViewModel
class TimelineViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(TimelineUiState(isLoading = true))
    val uiState: StateFlow<TimelineUiState> = _uiState.asStateFlow()

    init {
        loadMessages()
    }

    fun selectTab(tab: TimelineTab) {
        _uiState.value = _uiState.value.copy(activeTab = tab)
        loadMessages()
    }

    fun loadMessages() {
        val kind = when (_uiState.value.activeTab) {
            TimelineTab.PUBLIC -> "public"
            TimelineTab.FRIEND_GROUP -> "friend_group"
        }
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            try {
                val messages = repository.listMessages(kind = kind, limit = 50u)
                _uiState.value = _uiState.value.copy(isLoading = false, messages = messages)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(isLoading = false, error = e.message)
            }
        }
    }

    fun sync() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSyncing = true, error = null)
            try {
                repository.sync()
                val kind = when (_uiState.value.activeTab) {
                    TimelineTab.PUBLIC -> "public"
                    TimelineTab.FRIEND_GROUP -> "friend_group"
                }
                val messages = repository.listMessages(kind = kind, limit = 50u)
                _uiState.value = _uiState.value.copy(isSyncing = false, messages = messages)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(isSyncing = false, error = e.message)
            }
        }
    }
}
