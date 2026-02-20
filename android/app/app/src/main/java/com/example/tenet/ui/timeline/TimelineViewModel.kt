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

data class TimelineUiState(
    val messages: List<FfiMessage> = emptyList(),
    val isLoading: Boolean = false,
    val isSyncing: Boolean = false,
    val error: String? = null,
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

    fun loadMessages() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            try {
                val messages = repository.listMessages(kind = "public", limit = 50u)
                _uiState.value = TimelineUiState(messages = messages)
            } catch (e: Exception) {
                _uiState.value = TimelineUiState(error = e.message)
            }
        }
    }

    fun sync() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSyncing = true, error = null)
            try {
                repository.sync()
                val messages = repository.listMessages(kind = "public", limit = 50u)
                _uiState.value = TimelineUiState(messages = messages)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(isSyncing = false, error = e.message)
            }
        }
    }
}
