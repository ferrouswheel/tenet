package com.example.tenet.ui.conversations

import androidx.lifecycle.SavedStateHandle
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

data class ConversationDetailUiState(
    val messages: List<FfiMessage> = emptyList(),
    val isLoading: Boolean = false,
    val isSending: Boolean = false,
    val error: String? = null,
)

@HiltViewModel
class ConversationDetailViewModel @Inject constructor(
    savedStateHandle: SavedStateHandle,
    private val repository: TenetRepository,
) : ViewModel() {

    val peerId: String = checkNotNull(savedStateHandle["peerId"])

    private val _uiState = MutableStateFlow(ConversationDetailUiState(isLoading = true))
    val uiState: StateFlow<ConversationDetailUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            try {
                val messages = repository.listDirectMessages(peerId)
                _uiState.value = ConversationDetailUiState(messages = messages)
            } catch (e: Exception) {
                _uiState.value = ConversationDetailUiState(error = e.message)
            }
        }
    }

    fun send(body: String) {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSending = true, error = null)
            try {
                repository.sendDirect(peerId, body)
                val messages = repository.listDirectMessages(peerId)
                _uiState.value = ConversationDetailUiState(messages = messages)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(isSending = false, error = e.message)
            }
        }
    }
}
