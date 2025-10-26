let refreshTimer = null;
let isLoadingBatches = false;

// State preservation
let uiState = {
    expandedRows: new Set(),
    expandedReasoning: new Set(),
    textSelection: null
};

function saveCurrentUIState() {
    // Save text selection
    const selection = window.getSelection();
    if (selection.rangeCount > 0) {
        const range = selection.getRangeAt(0);
        const container = range.commonAncestorContainer;

        // Find the closest element with an ID that we can use to restore selection
        let element = container.nodeType === Node.TEXT_NODE ? container.parentElement : container;
        while (element && !element.id) {
            element = element.parentElement;
        }

        if (element && element.id) {
            const startPath = getNodePath(range.startContainer, element);
            const endPath = getNodePath(range.endContainer, element);

            // Only save selection if both paths are valid
            if (startPath !== null && endPath !== null) {
                uiState.textSelection = {
                    elementId: element.id,
                    startOffset: range.startOffset,
                    endOffset: range.endOffset,
                    startContainerPath: startPath,
                    endContainerPath: endPath
                };
            } else {
                uiState.textSelection = null;
            }
        } else {
            uiState.textSelection = null;
        }
    } else {
        uiState.textSelection = null;
    }
}

function getNodePath(node, root) {
    const path = [];
    let current = node;

    // Verify that root is actually an ancestor of node
    if (!root.contains(node) && root !== node) {
        return null; // Return null if root is not an ancestor
    }

    while (current && current !== root) {
        const parent = current.parentNode;
        if (parent) {
            const index = Array.from(parent.childNodes).indexOf(current);
            path.unshift(index);
        }
        current = parent;
    }
    return path;
}

function getNodeByPath(root, path) {
    let current = root;
    for (const index of path) {
        if (current.childNodes[index]) {
            current = current.childNodes[index];
        } else {
            return null;
        }
    }
    return current;
}

function restoreUIState() {
    // Restore expanded rows
    uiState.expandedRows.forEach(batchId => {
        // batchId stored in state needs to be normalized for DOM lookup
        const normalizedBatchId = normalizeId(batchId);
        const content = document.getElementById(`review-${normalizedBatchId}`);
        const button = document.getElementById(`btn-${normalizedBatchId}`);
        if (content && button) {
            content.classList.add('expanded');
            button.textContent = '▼ Hide';
        }
    });

    // Restore expanded reasoning
    uiState.expandedReasoning.forEach(batchId => {
        // batchId stored in state needs to be normalized for DOM lookup
        const normalizedBatchId = normalizeId(batchId);
        const reasoning = document.getElementById(`reasoning-${normalizedBatchId}`);
        const button = document.getElementById(`reasoning-btn-${normalizedBatchId}`);
        if (reasoning && button) {
            reasoning.classList.remove('hidden');
            button.textContent = 'Hide Reasoning';
        }
    });

    // Restore text selection
    if (uiState.textSelection) {
        setTimeout(() => {
            const element = document.getElementById(uiState.textSelection.elementId);
            if (element) {
                const startNode = getNodeByPath(element, uiState.textSelection.startContainerPath);
                const endNode = getNodeByPath(element, uiState.textSelection.endContainerPath);

                if (startNode && endNode) {
                    const selection = window.getSelection();
                    const range = document.createRange();
                    try {
                        range.setStart(startNode, uiState.textSelection.startOffset);
                        range.setEnd(endNode, uiState.textSelection.endOffset);
                        selection.removeAllRanges();
                        selection.addRange(range);
                    } catch (e) {
                        // Selection restoration failed, clear saved state
                        uiState.textSelection = null;
                    }
                }
            }
        }, 100); // Small delay to ensure DOM is fully rendered
    }
}

// Load API key and settings on page load
window.onload = function() {
    const savedKey = localStorage.getItem('openai_api_key');
    if (savedKey) {
        document.getElementById('apiKey').value = savedKey;
        loadBatches();
    } else {
        openSettingsModal();
    }

    // Load display cancelled jobs setting (default to false)
    const displayCancelled = localStorage.getItem('display_cancelled_jobs');
    document.getElementById('displayCancelledJobs').checked = displayCancelled === 'true';

    updateRefreshInterval();

    // Pause refresh when tab is hidden
    document.addEventListener('visibilitychange', function() {
        if (document.hidden) {
            console.log('Tab hidden, pausing auto-refresh');
            // Clear the interval when tab is hidden
            if (refreshTimer) {
                clearInterval(refreshTimer);
                refreshTimer = null;
            }
        } else {
            console.log('Tab visible, restarting auto-refresh');
            // Restart the interval when tab becomes visible
            updateRefreshInterval();
            // Also immediately refresh to get any new data
            if (!isLoadingBatches) {
                loadBatches();
            }
        }
    });

    // Set up event delegation for dynamically added buttons
    document.addEventListener('click', function(e) {
        if (e.target.classList.contains('review-toggle-btn')) {
            const batchId = e.target.getAttribute('data-batch-id');
            if (batchId) {
                toggleReview(batchId);
            }
        } else if (e.target.classList.contains('reasoning-toggle-btn')) {
            const batchId = e.target.getAttribute('data-batch-id');
            if (batchId) {
                toggleReasoning(batchId);
            }
        }
    });

    document.addEventListener('change', function(e) {
        if (e.target.classList.contains('resolved-checkbox')) {
            const batchId = e.target.getAttribute('data-batch-id');
            if (batchId) {
                toggleResolved(batchId);
            }
        }
    });
};

function openSettingsModal() {
    document.getElementById('settingsModal').classList.remove('hidden');
}

function closeSettingsModal() {
    document.getElementById('settingsModal').classList.add('hidden');
}

function saveSettings() {
    // Save display cancelled jobs setting (always save, independent of API key)
    const displayCancelled = document.getElementById('displayCancelledJobs').checked;
    localStorage.setItem('display_cancelled_jobs', displayCancelled.toString());

    const apiKey = document.getElementById('apiKey').value.trim();
    if (apiKey) {
        localStorage.setItem('openai_api_key', apiKey);
    }

    // Always show success message and close modal after saving
    showMessage('Settings saved', 'success');
    closeSettingsModal();

    // Refresh batches if we have a valid API key (from input or localStorage)
    if (getApiKey()) {
        loadBatches();
    }
}

function getApiKey() {
    return document.getElementById('apiKey').value.trim() || localStorage.getItem('openai_api_key');
}

function getDisplayCancelledJobs() {
    const setting = localStorage.getItem('display_cancelled_jobs');
    return setting === 'true';
}

function getResolvedState(batchId) {
    const resolved = localStorage.getItem('robocop_resolved') || '{}';
    try {
        return JSON.parse(resolved)[batchId] || false;
    } catch (e) {
        return false;
    }
}

function setResolvedState(batchId, resolved) {
    const current = localStorage.getItem('robocop_resolved') || '{}';
    try {
        const resolvedData = JSON.parse(current);
        resolvedData[batchId] = resolved;
        localStorage.setItem('robocop_resolved', JSON.stringify(resolvedData));
    } catch (e) {
        const resolvedData = {};
        resolvedData[batchId] = resolved;
        localStorage.setItem('robocop_resolved', JSON.stringify(resolvedData));
    }
}

function showMessage(message, type = 'error') {
    const errorDiv = document.getElementById('error');
    const textElement = errorDiv.querySelector('p');
    textElement.textContent = message;

    if (type === 'success') {
        errorDiv.className = 'bg-green-50 border border-green-200 rounded-lg p-4 mb-6';
        textElement.className = 'text-green-800';
    } else {
        errorDiv.className = 'bg-red-50 border border-red-200 rounded-lg p-4 mb-6';
        textElement.className = 'text-red-800';
    }

    errorDiv.classList.remove('hidden');
    setTimeout(() => errorDiv.classList.add('hidden'), 5000);
}

function updateRefreshInterval() {
    const interval = parseInt(document.getElementById('refreshInterval').value);
    console.log(`Updating refresh interval to ${interval} seconds`);
    if (refreshTimer) {
        clearInterval(refreshTimer);
        refreshTimer = null;
        console.log('Cleared existing refresh timer');
    }
    if (interval > 0 && !document.hidden) {
        refreshTimer = setInterval(loadBatches, interval * 1000);
        console.log(`Set new refresh timer: ${interval * 1000}ms`);
    } else if (interval > 0 && document.hidden) {
        console.log('Not setting timer - tab is hidden');
    } else {
        console.log('Auto-refresh disabled');
    }
}

function getRelativeTime(timestamp) {
    const seconds = Math.floor((Date.now() / 1000) - timestamp);
    if (seconds < 60) return 'just now';
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
    if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
    return `${Math.floor(seconds / 86400)}d ago`;
}

function getStatusBadge(status) {
    const colors = {
        'completed': 'bg-green-100 text-green-800',
        'failed': 'bg-red-100 text-red-800',
        'expired': 'bg-red-100 text-red-800',
        'in_progress': 'bg-yellow-100 text-yellow-800',
        'validating': 'bg-yellow-100 text-yellow-800',
        'finalizing': 'bg-yellow-100 text-yellow-800',
        'cancelling': 'bg-gray-100 text-gray-800',
        'cancelled': 'bg-gray-100 text-gray-800'
    };
    const color = colors[status] || 'bg-gray-100 text-gray-800';
    const escapedStatus = escapeHtml(status);
    return `<span class="px-2 py-1 text-xs font-semibold rounded-full ${color}">${escapedStatus}</span>`;
}

function formatCommitHash(hash, full = false) {
    if (!hash) return 'N/A';
    const short = hash.substring(0, 7);
    // Always returns plain text (never HTML) for consistent behavior
    return full ? hash : short;
}

function toggleReview(batchId) {
    const normalizedBatchId = normalizeId(batchId);
    const content = document.getElementById(`review-${normalizedBatchId}`);
    const button = document.getElementById(`btn-${normalizedBatchId}`);
    content.classList.toggle('expanded');
    button.textContent = content.classList.contains('expanded') ? '▼ Hide' : '▶ Show';

    // Update state tracking - use raw batchId for storage
    if (content.classList.contains('expanded')) {
        uiState.expandedRows.add(batchId);
    } else {
        uiState.expandedRows.delete(batchId);
    }
}

function toggleReasoning(batchId) {
    const normalizedBatchId = normalizeId(batchId);
    const reasoning = document.getElementById(`reasoning-${normalizedBatchId}`);
    const button = document.getElementById(`reasoning-btn-${normalizedBatchId}`);
    reasoning.classList.toggle('hidden');
    button.textContent = reasoning.classList.contains('hidden') ? 'Show Reasoning' : 'Hide Reasoning';

    // Update state tracking - use raw batchId for storage
    if (!reasoning.classList.contains('hidden')) {
        uiState.expandedReasoning.add(batchId);
    } else {
        uiState.expandedReasoning.delete(batchId);
    }
}

function toggleResolved(batchId) {
    const currentState = getResolvedState(batchId);
    setResolvedState(batchId, !currentState);

    // Update the checkbox
    const normalizedBatchId = normalizeId(batchId);
    const checkbox = document.getElementById(`resolved-${normalizedBatchId}`);
    if (checkbox) {
        checkbox.checked = !currentState;
    }

    // Update the row styling
    const row = checkbox?.closest('tr');
    if (row) {
        if (!currentState) {
            // Now resolved
            row.className = 'resolved-row';
        } else {
            // No longer resolved
            row.className = 'hover:bg-gray-50';
        }
    }
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}

function normalizeId(id) {
    // Create a collision-resistant safe HTML ID using base64url encoding
    // This avoids collisions that can occur with simple character replacement
    try {
        // Convert to base64 and make it URL-safe
        const base64 = btoa(id)
            .replace(/\+/g, '-')
            .replace(/\//g, '_')
            .replace(/=/g, '');
        // Prefix with 'id_' to ensure it starts with a letter
        return 'id_' + base64;
    } catch (e) {
        // Fallback to simple replacement if btoa fails
        return 'id_' + id.replace(/[^a-zA-Z0-9\-_]/g, '_');
    }
}

function createSafeRepoLink(repoName, url, pullRequestUrl) {
    // Sanitize pull request URL if provided, otherwise sanitize repo URL
    const safePrUrl = pullRequestUrl ? getSafeRepoUrl(pullRequestUrl) : null;
    const safeUrl = url ? getSafeRepoUrl(url) : null;
    const linkUrl = safePrUrl || safeUrl;

    if (!linkUrl) {
        return escapeHtml(repoName || 'N/A');
    }

    // Create anchor element safely using DOM methods
    const link = document.createElement('a');
    link.href = linkUrl; // href assignment does not sanitize protocols
    link.textContent = repoName || 'N/A'; // textContent avoids HTML injection
    link.target = '_blank';
    link.rel = 'noopener noreferrer';
    link.className = 'text-blue-600 hover:text-blue-800 hover:underline';

    return link.outerHTML;
}

function getSafeRepoUrl(remoteUrl) {
    if (!remoteUrl) return null;

    // Only allow http/https URLs
    if (/^https?:\/\//i.test(remoteUrl)) {
        return remoteUrl;
    }

    // Normalize common SSH-style URLs to HTTPS
    // Handle git@github.com:org/repo.git -> https://github.com/org/repo
    const sshMatch = remoteUrl.match(/^git@([^:]+):(.+?)(?:\.git)?$/);
    if (sshMatch) {
        const [, host, path] = sshMatch;
        if (host === 'github.com' || host === 'gitlab.com' || host === 'bitbucket.org') {
            return `https://${host}/${path}`;
        }
    }

    // Handle ssh://git@host/path -> https://host/path
    const sshProtocolMatch = remoteUrl.match(/^ssh:\/\/(?:git@)?([^\/]+)\/(.+?)(?:\.git)?$/);
    if (sshProtocolMatch) {
        const [, host, path] = sshProtocolMatch;
        if (host === 'github.com' || host === 'gitlab.com' || host === 'bitbucket.org') {
            return `https://${host}/${path}`;
        }
    }

    // If we can't safely convert it, return null (no link)
    return null;
}

async function loadBatches() {
    if (isLoadingBatches) {
        console.log('Skipping loadBatches - already in progress');
        return; // Prevent overlapping loads
    }

    const apiKey = getApiKey();
    if (!apiKey) {
        showMessage('Please enter an API key');
        return;
    }

    console.log('Starting loadBatches');
    isLoadingBatches = true;

    // Save current UI state before reloading
    saveCurrentUIState();

    const loading = document.getElementById('loading');
    const refreshBtn = document.getElementById('refreshBtn');
    loading.classList.remove('hidden');
    refreshBtn.disabled = true;

    try {
        // Fetch batches
        const response = await fetch(`${API_BASE}/batches?limit=100`, {
            headers: {
                'Authorization': `Bearer ${apiKey}`,
                'Content-Type': 'application/json'
            }
        });

        if (!response.ok) {
            throw new Error(`API error: ${response.statusText}`);
        }

        const data = await response.json();

        // Filter for robocop batches with schema version 1
        const roboCopBatches = data.data.filter(batch =>
            batch.metadata?.description === 'robocop code review tool' &&
            batch.metadata?.metadata_schema === '1'
        );

        console.log(`Found ${roboCopBatches.length} robocop batches`);

        // Cache for completed batch review data
        const cachedReviews = JSON.parse(localStorage.getItem('robocop_review_cache') || '{}');
        const cachedErrors = JSON.parse(localStorage.getItem('robocop_error_cache') || '{}');

        // Detect errors in batches
        roboCopBatches.forEach(batch => {
            // Check for errors using request_counts.failed or presence of error_file_id
            const failedCount = batch.request_counts?.failed || 0;
            batch.hasErrors = failedCount > 0 || !!batch.error_file_id;
            batch.failedCount = failedCount;
            batch.errorFileId = batch.error_file_id;
        });

        // Fetch output for completed batches in parallel (only if not already cached)
        const outputPromises = roboCopBatches
            .filter(batch => batch.status === 'completed' && batch.output_file_id)
            .map(async batch => {
                // Check if we already have cached review data for this batch
                if (cachedReviews[batch.id]) {
                    batch.reviewData = cachedReviews[batch.id];
                    return;
                }

                try {
                    const output = await fetchBatchOutput(batch.output_file_id);
                    batch.reviewData = parseReviewFromOutput(output);

                    // Cache the review data for completed batches
                    if (batch.reviewData) {
                        cachedReviews[batch.id] = batch.reviewData;
                        localStorage.setItem('robocop_review_cache', JSON.stringify(cachedReviews));
                    }
                } catch (e) {
                    console.error(`Failed to fetch output for batch ${batch.id}:`, e);
                }
            });

        // Fetch error files for batches with errors in parallel (only if not already cached)
        const errorPromises = roboCopBatches
            .filter(batch => batch.hasErrors && batch.errorFileId)
            .map(async batch => {
                // Check if we already have cached error data for this batch
                if (cachedErrors[batch.id]) {
                    batch.errorData = cachedErrors[batch.id];
                    return;
                }

                try {
                    const errorText = await fetchBatchError(batch.errorFileId);
                    batch.errorData = parseErrorFromOutput(errorText);

                    // Cache the error data
                    if (batch.errorData) {
                        cachedErrors[batch.id] = batch.errorData;
                        localStorage.setItem('robocop_error_cache', JSON.stringify(cachedErrors));
                    }
                } catch (e) {
                    console.error(`Failed to fetch error file for batch ${batch.id}:`, e);
                }
            });

        await Promise.all([...outputPromises, ...errorPromises]);

        renderBatches(roboCopBatches);

        // Restore UI state after rendering
        restoreUIState();

        document.getElementById('lastUpdate').textContent = `Last updated: ${new Date().toLocaleTimeString()}`;
        console.log('loadBatches completed successfully');

    } catch (error) {
        console.error('Error loading batches:', error);
        showMessage(`Error: ${error.message}`);
    } finally {
        loading.classList.add('hidden');
        refreshBtn.disabled = false;
        isLoadingBatches = false;
        console.log('loadBatches finished, isLoadingBatches reset to false');
    }
}
