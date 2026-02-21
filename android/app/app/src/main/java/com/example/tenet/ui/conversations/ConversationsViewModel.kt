package com.example.tenet.ui.conversations

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiConversation
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class ConversationsUiState(
    val conversations: List<FfiConversation> = emptyList(),
    val isLoading: Boolean = false,
    val error: String? = null,
)

@HiltViewModel
class ConversationsViewModel @Inject constructor(
    private val repository: TenetRepository,
) : ViewModel() {

    private val _uiState = MutableStateFlow(ConversationsUiState(isLoading = true))
    val uiState: StateFlow<ConversationsUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            try {
                val conversations = repository.listConversations()
                _uiState.value = ConversationsUiState(conversations = conversations)
            } catch (e: Exception) {
                _uiState.value = ConversationsUiState(error = e.message)
            }
        }
    }
}
