package com.example.tenet.ui.postdetail

import androidx.lifecycle.SavedStateHandle
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.example.tenet.data.TenetRepository
import com.example.tenet.uniffi.FfiMessage
import com.example.tenet.uniffi.FfiReactionSummary
import dagger.hilt.android.lifecycle.HiltViewModel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import javax.inject.Inject

data class PostDetailUiState(
    val parent: FfiMessage? = null,
    val replies: List<FfiMessage> = emptyList(),
    val reactions: FfiReactionSummary? = null,
    val isLoading: Boolean = false,
    val isSending: Boolean = false,
    val error: String? = null,
)

@HiltViewModel
class PostDetailViewModel @Inject constructor(
    savedStateHandle: SavedStateHandle,
    private val repository: TenetRepository,
) : ViewModel() {

    val messageId: String = checkNotNull(savedStateHandle["messageId"])

    private val _uiState = MutableStateFlow(PostDetailUiState(isLoading = true))
    val uiState: StateFlow<PostDetailUiState> = _uiState.asStateFlow()

    init {
        load()
    }

    fun load() {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isLoading = true, error = null)
            try {
                val parent = repository.getMessage(messageId)
                val replies = repository.listReplies(messageId)
                val reactions = repository.getReactions(messageId)
                _uiState.value = PostDetailUiState(
                    parent = parent,
                    replies = replies,
                    reactions = reactions,
                )
            } catch (e: Exception) {
                _uiState.value = PostDetailUiState(error = e.message)
            }
        }
    }

    fun reply(body: String) {
        viewModelScope.launch {
            _uiState.value = _uiState.value.copy(isSending = true, error = null)
            try {
                repository.replyTo(messageId, body)
                val replies = repository.listReplies(messageId)
                _uiState.value = _uiState.value.copy(isSending = false, replies = replies)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(isSending = false, error = e.message)
            }
        }
    }

    fun react(reaction: String) {
        viewModelScope.launch {
            try {
                val summary = repository.react(messageId, reaction)
                _uiState.value = _uiState.value.copy(reactions = summary)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(error = e.message)
            }
        }
    }

    fun unreact() {
        viewModelScope.launch {
            try {
                val summary = repository.unreact(messageId)
                _uiState.value = _uiState.value.copy(reactions = summary)
            } catch (e: Exception) {
                _uiState.value = _uiState.value.copy(error = e.message)
            }
        }
    }
}
