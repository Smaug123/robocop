function renderReview(batch) {
    // Handle HTTP error status codes (e.g., 503 Service Unavailable)
    if (batch.reviewData?.isHttpError) {
        const normalizedBatchId = normalizeId(batch.id);
        const rawBatchId = batch.id;
        const statusCode = batch.reviewData.statusCode;
        const requestId = batch.reviewData.requestId;

        return `
            <button
                id="btn-${normalizedBatchId}"
                data-batch-id="${rawBatchId}"
                class="text-blue-600 hover:text-blue-800 font-medium text-sm review-toggle-btn"
            >
                ▶ Show
            </button>
            <span class="text-red-600 ml-2 text-sm font-semibold">HTTP Error ${statusCode}</span>
            <div id="review-${normalizedBatchId}" class="review-content mt-2">
                <div class="bg-red-50 rounded p-3 mt-2 text-sm border border-red-200">
                    <div class="mb-2">
                        <strong class="text-red-800">HTTP Error Response</strong>
                    </div>
                    <div class="mb-2">
                        <strong class="text-gray-700">Status Code:</strong>
                        <span class="text-red-600 font-semibold">${statusCode}</span>
                    </div>
                    <div class="mb-2">
                        <strong class="text-gray-700">Request ID:</strong>
                        <code class="text-xs bg-white px-1 py-0.5 rounded">${escapeHtml(requestId)}</code>
                    </div>
                </div>
            </div>
        `;
    }

    // Handle error state - batch completed but had errors
    if (batch.hasErrors) {
        const normalizedBatchId = normalizeId(batch.id);
        const rawBatchId = batch.id;
        const failedCount = batch.failedCount || 0;
        const totalCount = batch.request_counts?.total || 0;
        const errorFileId = batch.errorFileId || 'N/A';

        // Build error details HTML
        let errorDetailsHtml = '';
        if (batch.errorData && batch.errorData.length > 0) {
            errorDetailsHtml = batch.errorData.map(error => {
                const escapedMessage = escapeHtml(error.message);
                const escapedCode = escapeHtml(error.code);

                // If this is an HTTP error, show status code prominently
                if (error.statusCode) {
                    const escapedRequestId = escapeHtml(error.requestId || 'N/A');
                    return `<div class="mb-2 p-2 bg-white rounded border border-red-200">
                        <div><strong>HTTP Status Code:</strong> <span class="text-red-600 font-semibold">${error.statusCode}</span></div>
                        <div><strong>Request ID:</strong> <code class="text-xs bg-gray-50 px-1 py-0.5 rounded">${escapedRequestId}</code></div>
                        <div class="text-gray-600 text-sm mt-1">${escapedMessage}</div>
                    </div>`;
                }

                // Traditional error format
                return `<div class="mb-2 p-2 bg-white rounded border border-red-200">
                    <div><strong>Error Code:</strong> ${escapedCode}</div>
                    <div><strong>Message:</strong> ${escapedMessage}</div>
                </div>`;
            }).join('');
        } else {
            errorDetailsHtml = '<div class="text-gray-600 text-sm">Error details not available</div>';
        }

        return `
            <button
                id="btn-${normalizedBatchId}"
                data-batch-id="${rawBatchId}"
                class="text-blue-600 hover:text-blue-800 font-medium text-sm review-toggle-btn"
            >
                ▶ Show
            </button>
            <span class="text-red-600 ml-2 text-sm font-semibold">Error (${failedCount}/${totalCount} failed)</span>
            <div id="review-${normalizedBatchId}" class="review-content mt-2">
                <div class="bg-red-50 rounded p-3 mt-2 text-sm border border-red-200">
                    <div class="mb-2">
                        <strong class="text-red-800">Batch Processing Error</strong>
                    </div>
                    <div class="mb-2">
                        <strong class="text-gray-700">Error File ID:</strong>
                        <code class="text-xs bg-white px-1 py-0.5 rounded">${escapeHtml(errorFileId)}</code>
                    </div>
                    <div class="mb-2">
                        <strong class="text-gray-700">Failed Requests:</strong>
                        <span class="text-red-600 font-semibold">${failedCount} of ${totalCount}</span>
                    </div>
                    <div class="mt-3">
                        <strong class="text-gray-700">Error Details:</strong>
                        <div class="mt-2">
                            ${errorDetailsHtml}
                        </div>
                    </div>
                </div>
            </div>
        `;
    }

    // Handle success state - batch completed successfully
    const review = batch.reviewData;
    if (!review) {
        return '<span class="text-gray-400 text-sm">Review not available</span>';
    }

    const hasComments = review.substantiveComments;
    const summaryClass = hasComments ? 'text-orange-600 font-semibold' : 'text-green-600';
    const summaryText = hasComments ? 'Issues Found' : 'No Issues';

    // Escape model output to prevent XSS
    const escapedSummary = escapeHtml(review.summary || 'n/a');
    const escapedReasoning = escapeHtml(review.reasoning || 'N/A');

    // Normalize the batch ID for safe use in HTML IDs
    const normalizedBatchId = normalizeId(batch.id);
    // Use raw batch ID for data attributes to avoid mismatch issues
    const rawBatchId = batch.id;

    return `
        <button
            id="btn-${normalizedBatchId}"
            data-batch-id="${rawBatchId}"
            class="text-blue-600 hover:text-blue-800 font-medium text-sm review-toggle-btn"
        >
            ▶ Show
        </button>
        <span class="${summaryClass} ml-2 text-sm">${summaryText}</span>
        <div id="review-${normalizedBatchId}" class="review-content mt-2">
            <div class="bg-gray-50 rounded p-3 mt-2 text-sm">
                <div class="mb-2">
                    <strong class="text-gray-700">Substantive Comments:</strong>
                    <span class="${hasComments ? 'text-orange-600' : 'text-green-600'} font-semibold">
                        ${hasComments ? 'Yes' : 'No'}
                    </span>
                </div>
                <div class="mb-2">
                    <strong class="text-gray-700">Summary:</strong>
                    <pre class="mt-1 text-gray-800">${escapedSummary}</pre>
                </div>
                <div class="mt-3">
                    <button
                        id="reasoning-btn-${normalizedBatchId}"
                        data-batch-id="${rawBatchId}"
                        class="text-sm text-blue-600 hover:text-blue-800 font-medium reasoning-toggle-btn"
                    >
                        Show Reasoning
                    </button>
                    <div id="reasoning-${normalizedBatchId}" class="hidden mt-2">
                        <strong class="text-gray-700">Reasoning:</strong>
                        <pre class="mt-1 text-gray-600 bg-white p-2 rounded border border-gray-200">${escapedReasoning}</pre>
                    </div>
                </div>
            </div>
        </div>
    `;
}

function renderBatches(batches) {
    const tbody = document.getElementById('batchesBody');

    // Filter out cancelled jobs if the setting is disabled
    // Note: "cancelling" status should still be displayed
    const displayCancelled = getDisplayCancelledJobs();
    const filteredBatches = displayCancelled
        ? batches
        : batches.filter(batch => batch.status !== 'cancelled');

    if (filteredBatches.length === 0) {
        tbody.innerHTML = `
            <tr>
                <td colspan="9" class="px-6 py-4 text-center text-gray-500">
                    No robocop review batches found
                </td>
            </tr>
        `;
        return;
    }

    tbody.innerHTML = filteredBatches.map(batch => {
        const normalizedBatchId = normalizeId(batch.id);
        const rawBatchId = batch.id;
        const isResolved = getResolvedState(batch.id);
        const rowClass = isResolved ? "resolved-row" : "hover:bg-gray-50";
        return `
        <tr class="${rowClass}">
            <td class="px-6 py-4 text-center">
                ${batch.status === 'completed' && batch.reviewData?.substantiveComments ?
                    `<input
                        type="checkbox"
                        id="resolved-${normalizedBatchId}"
                        data-batch-id="${rawBatchId}"
                        class="w-4 h-4 text-green-600 bg-gray-100 border-gray-300 rounded focus:ring-green-500 focus:ring-2 resolved-checkbox"
                    />` :
                    '<span class="text-gray-400">-</span>'
                }
            </td>
            <td class="px-6 py-4 whitespace-nowrap">
                ${getStatusBadge(batch.status)}
            </td>
            <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                <span title="${new Date(batch.created_at * 1000).toLocaleString()}">
                    ${getRelativeTime(batch.created_at)}
                </span>
            </td>
            <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900">
                ${escapeHtml(batch.metadata?.branch || 'N/A')}
            </td>
            <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900 font-medium">
                ${createSafeRepoLink(batch.metadata?.repo_name, batch.metadata?.remote_url, batch.metadata?.pull_request_url)}
            </td>
            <td class="px-6 py-4 whitespace-nowrap text-sm font-mono text-gray-900">
                <span title="${escapeHtml(batch.metadata?.source_commit || 'N/A')}">
                    ${formatCommitHash(batch.metadata?.source_commit)}
                </span>
            </td>
            <td class="px-6 py-4 whitespace-nowrap text-sm font-mono text-gray-900">
                <span title="${escapeHtml(batch.metadata?.target_commit || 'N/A')}">
                    ${formatCommitHash(batch.metadata?.target_commit)}
                </span>
            </td>
            <td class="px-6 py-4 whitespace-nowrap text-sm text-gray-900">
                <span title="Reasoning effort: ${escapeHtml(batch.metadata?.reasoning_effort || 'N/A')}&#10;Robocop version: ${escapeHtml(batch.metadata?.version || 'N/A')}">
                    ${escapeHtml(batch.metadata?.model || 'N/A')}
                </span>
            </td>
            <td class="px-6 py-4 text-sm">
                ${batch.status === 'completed' ? renderReview(batch) :
                  `<span class="text-gray-400">Pending (${escapeHtml(batch.status)})</span>`}
            </td>
        </tr>
    `;}).join('');

    // Set checkbox states after DOM is updated
    filteredBatches.forEach(batch => {
        if (batch.status === 'completed' && batch.reviewData?.substantiveComments) {
            const normalizedBatchId = normalizeId(batch.id);
            const checkbox = document.getElementById(`resolved-${normalizedBatchId}`);
            if (checkbox) {
                checkbox.checked = getResolvedState(batch.id);
            }
        }
    });
}
