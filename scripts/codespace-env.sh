#!/usr/bin/env bash

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  echo "source this file from another script" >&2
  exit 1
fi

codespace_url() {
  local port="$1"
  printf 'https://%s-%s.%s' "${CODESPACE_NAME}" "${port}" "${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN}"
}

if [[ -n "${CODESPACE_NAME:-}" && -n "${GITHUB_CODESPACES_PORT_FORWARDING_DOMAIN:-}" ]]; then
  export NEXT_PUBLIC_LANDING_PAGE_URL="${NEXT_PUBLIC_LANDING_PAGE_URL:-$(codespace_url 3000)}"
  export NEXT_PUBLIC_ADMIN_PORTAL_URL="${NEXT_PUBLIC_ADMIN_PORTAL_URL:-$(codespace_url 3003)}"
  export NEXT_PUBLIC_SIMLAB_URL="${NEXT_PUBLIC_SIMLAB_URL:-$(codespace_url 3010)/simulation}"
  export RAKSHA_WEB_OIDC_REDIRECT_URI="${RAKSHA_WEB_OIDC_REDIRECT_URI:-$(codespace_url 3000)/api/auth/callback}"
  export RAKSHA_WEB_POST_LOGOUT_REDIRECT_URI="${RAKSHA_WEB_POST_LOGOUT_REDIRECT_URI:-$(codespace_url 3000)/}"
  export RAKSHA_ADMIN_OIDC_REDIRECT_URI="${RAKSHA_ADMIN_OIDC_REDIRECT_URI:-$(codespace_url 3003)/api/auth/oidc/callback}"
  export RAKSHA_ADMIN_POST_LOGOUT_REDIRECT_URI="${RAKSHA_ADMIN_POST_LOGOUT_REDIRECT_URI:-$(codespace_url 3000)/}"
  export SIMLAB_API_PUBLIC_URL="${SIMLAB_API_PUBLIC_URL:-$(codespace_url 8010)}"
  export SIMLAB_PATTERN_EDITOR_URL="${SIMLAB_PATTERN_EDITOR_URL:-$(codespace_url 3003)/admin?tab=pattern_catalog&tenant_id={tenant_id}&pattern_id={pattern_id}}"
  export SIMLAB_CORS_ORIGINS="${SIMLAB_CORS_ORIGINS:-$(codespace_url 3010),http://frontend:3000}"
fi

