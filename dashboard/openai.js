const API_BASE = 'https://api.openai.com/v1';

async function fetchBatchOutput(fileId) {
    const apiKey = getApiKey();
    const response = await fetch(`${API_BASE}/files/${fileId}/content`, {
        headers: {
            'Authorization': `Bearer ${apiKey}`
        }
    });
    if (!response.ok) throw new Error(`Failed to fetch output file: ${response.statusText}`);
    return await response.text();
}

async function fetchBatchError(fileId) {
    const apiKey = getApiKey();
    const response = await fetch(`${API_BASE}/files/${fileId}/content`, {
        headers: {
            'Authorization': `Bearer ${apiKey}`
        }
    });
    if (!response.ok) throw new Error(`Failed to fetch error file: ${response.statusText}`);
    return await response.text();
}

function parseReviewFromOutput(outputText) {
    const lines = outputText.trim().split('\n');
    for (const line of lines) {
        if (!line.trim()) continue;

        try {
            const result = JSON.parse(line.trim());
            if (result.custom_id === 'robocop-review-1') {
                // Check for HTTP error status codes in the response
                if (result.response && typeof result.response.status_code === 'number' && result.response.status_code !== 200) {
                    return {
                        isHttpError: true,
                        statusCode: result.response.status_code,
                        requestId: result.response.request_id || 'N/A'
                    };
                }

                // Check for successful response - responses API format
                // Find the message output (there may be other outputs like reasoning)
                if (result.response?.body?.output) {
                    const messageOutput = result.response.body.output.find(o => o.type === 'message');
                    if (messageOutput?.content) {
                        const textContent = messageOutput.content.find(c => c.type === 'output_text');
                        if (textContent?.text) {
                            return parseJsonContent(textContent.text);
                        }
                    }
                }

                // Fallback: legacy chat completions format (for backwards compatibility)
                if (result.response?.body?.choices?.[0]?.message?.content) {
                    const content = result.response.body.choices[0].message.content;
                    return parseJsonContent(content);
                }
            }
        } catch (jsonError) {
            // Skip malformed JSON lines and continue processing other lines
            console.warn('Skipping malformed JSON line:', line.substring(0, 100));
            continue;
        }
    }

    // Log if no valid review was found in any line
    console.warn('No valid robocop review found in output');
    return null;
}

function parseJsonContent(content) {
    try {
        return JSON.parse(content);
    } catch (e) {
        console.warn('Failed to parse model content as JSON:', content.substring(0, 100), 'Error:', e.message);
        return null;
    }
}

function parseErrorFromOutput(errorText) {
    const lines = errorText.trim().split('\n');
    const errors = [];

    for (const line of lines) {
        if (!line.trim()) continue;

        try {
            const errorEntry = JSON.parse(line.trim());

            // Handle HTTP error status codes (e.g., {"response": {"status_code": 503, ...}, "error": null})
            if (errorEntry.response && typeof errorEntry.response.status_code === 'number' && errorEntry.response.status_code !== 200) {
                errors.push({
                    customId: errorEntry.custom_id,
                    message: `HTTP ${errorEntry.response.status_code} error`,
                    code: `http_${errorEntry.response.status_code}`,
                    statusCode: errorEntry.response.status_code,
                    requestId: errorEntry.response.request_id
                });
            }
            // Handle traditional error format: { "custom_id": "...", "error": { "message": "...", "code": "..." } }
            else if (errorEntry.error) {
                errors.push({
                    customId: errorEntry.custom_id,
                    message: errorEntry.error.message || 'Unknown error',
                    code: errorEntry.error.code || 'unknown'
                });
            }
        } catch (jsonError) {
            console.warn('Skipping malformed error JSON line:', line.substring(0, 100));
            continue;
        }
    }

    return errors.length > 0 ? errors : null;
}
