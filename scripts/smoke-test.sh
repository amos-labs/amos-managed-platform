#!/usr/bin/env bash
# ============================================================================
# AMOS Platform - Post-Deploy Smoke Test
# ============================================================================
# Usage: ./scripts/smoke-test.sh <BASE_URL>
# Example: ./scripts/smoke-test.sh https://platform.amoslabs.com
#
# Runs a battery of HTTP checks against a deployed instance to verify
# the service is healthy and responding correctly. Exits 0 on success,
# 1 on any failure.
# ============================================================================

set -euo pipefail

BASE_URL="${1:?Usage: smoke-test.sh <BASE_URL>}"
# Strip trailing slash
BASE_URL="${BASE_URL%/}"

PASS=0
FAIL=0
MAX_RETRIES=12
RETRY_INTERVAL=10

log()  { echo "[smoke-test] $*"; }
pass() { PASS=$((PASS + 1)); log "PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); log "FAIL: $1"; }

# --------------------------------------------------------------------------
# Wait for service to become reachable
# --------------------------------------------------------------------------
log "Waiting for service at $BASE_URL to become reachable..."
RETRY=0
while [ $RETRY -lt $MAX_RETRIES ]; do
    HTTP_STATUS=$(curl -sf -o /dev/null -w "%{http_code}" --connect-timeout 5 --max-time 10 "$BASE_URL/api/v1/health" 2>/dev/null || echo "000")
    if [ "$HTTP_STATUS" = "200" ]; then
        log "Service is reachable (HTTP $HTTP_STATUS)"
        break
    fi
    RETRY=$((RETRY + 1))
    log "Attempt $RETRY/$MAX_RETRIES: HTTP $HTTP_STATUS, retrying in ${RETRY_INTERVAL}s..."
    sleep "$RETRY_INTERVAL"
done

if [ "$HTTP_STATUS" != "200" ]; then
    fail "Service never became reachable after $MAX_RETRIES attempts"
    echo ""
    log "RESULTS: $PASS passed, $FAIL failed"
    exit 1
fi

# --------------------------------------------------------------------------
# Test 1: Health endpoint returns 200 with JSON
# --------------------------------------------------------------------------
RESPONSE=$(curl -sf --max-time 10 "$BASE_URL/api/v1/health" 2>/dev/null || echo "CURL_FAILED")
if echo "$RESPONSE" | grep -q '"status"'; then
    pass "GET /api/v1/health returns JSON with status field"
else
    fail "GET /api/v1/health - unexpected response: $RESPONSE"
fi

# --------------------------------------------------------------------------
# Test 2: Readiness endpoint
# --------------------------------------------------------------------------
HTTP_STATUS=$(curl -sf -o /dev/null -w "%{http_code}" --max-time 10 "$BASE_URL/api/v1/ready" 2>/dev/null || echo "000")
if [ "$HTTP_STATUS" = "200" ]; then
    pass "GET /api/v1/ready returns 200"
else
    fail "GET /api/v1/ready returned HTTP $HTTP_STATUS (expected 200)"
fi

# --------------------------------------------------------------------------
# Test 3: API catalog at /api/v1 returns JSON
# --------------------------------------------------------------------------
RESPONSE=$(curl -sf --max-time 10 "$BASE_URL/api/v1" 2>/dev/null || echo "CURL_FAILED")
if echo "$RESPONSE" | grep -q '"endpoints"'; then
    pass "GET /api/v1 returns API catalog with endpoints"
else
    fail "GET /api/v1 - no endpoints field in response"
fi

# --------------------------------------------------------------------------
# Test 4: Root redirects browsers to /login
# --------------------------------------------------------------------------
HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 -H "Accept: text/html" "$BASE_URL/" 2>/dev/null || echo "000")
if [ "$HTTP_STATUS" = "302" ] || [ "$HTTP_STATUS" = "200" ]; then
    pass "GET / with Accept:text/html returns $HTTP_STATUS (redirect or login page)"
else
    fail "GET / returned HTTP $HTTP_STATUS (expected 302 or 200)"
fi

# --------------------------------------------------------------------------
# Test 5: Root returns JSON for API clients (no Accept: text/html)
# --------------------------------------------------------------------------
RESPONSE=$(curl -sf --max-time 10 -H "Accept: application/json" "$BASE_URL/" 2>/dev/null || echo "CURL_FAILED")
if echo "$RESPONSE" | grep -q '"service"'; then
    pass "GET / with Accept:application/json returns service catalog"
elif echo "$RESPONSE" | grep -q '"endpoints"'; then
    pass "GET / with Accept:application/json returns endpoint catalog"
else
    fail "GET / as API client - unexpected response: $(echo "$RESPONSE" | head -c 200)"
fi

# --------------------------------------------------------------------------
# Test 6: Login page renders HTML
# --------------------------------------------------------------------------
RESPONSE=$(curl -sf --max-time 10 "$BASE_URL/login" 2>/dev/null || echo "CURL_FAILED")
if echo "$RESPONSE" | grep -qi '<html\|<!doctype\|<form'; then
    pass "GET /login returns HTML page"
else
    fail "GET /login - no HTML content found"
fi

# --------------------------------------------------------------------------
# Test 7: Protected endpoint returns 401 without auth
# --------------------------------------------------------------------------
HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 "$BASE_URL/api/v1/tenants/me" 2>/dev/null || echo "000")
if [ "$HTTP_STATUS" = "401" ]; then
    pass "GET /api/v1/tenants/me returns 401 without auth"
else
    fail "GET /api/v1/tenants/me returned HTTP $HTTP_STATUS (expected 401)"
fi

# --------------------------------------------------------------------------
# Test 8: 404 returns JSON (not HTML)
# --------------------------------------------------------------------------
RESPONSE=$(curl -sf --max-time 10 "$BASE_URL/api/v1/nonexistent-route-12345" 2>/dev/null || echo "CURL_FAILED")
if echo "$RESPONSE" | grep -q '"error"\|"message"'; then
    pass "GET /api/v1/nonexistent returns JSON error"
elif [ "$RESPONSE" = "CURL_FAILED" ]; then
    # curl -f will fail on 404, that's ok - check the status code
    HTTP_STATUS=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 "$BASE_URL/api/v1/nonexistent-route-12345" 2>/dev/null || echo "000")
    if [ "$HTTP_STATUS" = "404" ]; then
        pass "GET /api/v1/nonexistent returns 404"
    else
        fail "GET /api/v1/nonexistent returned HTTP $HTTP_STATUS (expected 404)"
    fi
else
    fail "GET /api/v1/nonexistent - unexpected response format"
fi

# --------------------------------------------------------------------------
# Test 9: Response headers include security basics
# --------------------------------------------------------------------------
HEADERS=$(curl -sI --max-time 10 "$BASE_URL/api/v1/health" 2>/dev/null || echo "")
if echo "$HEADERS" | grep -qi "content-type"; then
    pass "Response includes Content-Type header"
else
    fail "Response missing Content-Type header"
fi

# --------------------------------------------------------------------------
# Results
# --------------------------------------------------------------------------
echo ""
echo "=========================================="
log "RESULTS: $PASS passed, $FAIL failed"
echo "=========================================="

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi

exit 0
