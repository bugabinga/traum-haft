# oauth2-proxy settings for the e2e run. Mirrors locals.oauth2_proxy_env and
# locals.oauth2_proxy_connect_env in isp-insoft-cloud/modules/traum-haft;
# only issuer, client and addresses differ.
common() {
  export OAUTH2_PROXY_PROVIDER=oidc
  export OAUTH2_PROXY_OIDC_ISSUER_URL="$MOCK_GOOGLE"
  export OAUTH2_PROXY_CLIENT_ID=e2e-client
  export OAUTH2_PROXY_CLIENT_SECRET=e2e-secret
  export OAUTH2_PROXY_COOKIE_SECRET=0123456789abcdef0123456789abcdef
  export OAUTH2_PROXY_COOKIE_SECURE=true
  export OAUTH2_PROXY_COOKIE_SAMESITE=lax
  export OAUTH2_PROXY_COOKIE_EXPIRE=24h
  export OAUTH2_PROXY_EMAIL_DOMAINS=isp-insoft.de
  export OAUTH2_PROXY_OIDC_GROUPS_CLAIM=hd
  export OAUTH2_PROXY_ALLOWED_GROUPS=isp-insoft.de
  export OAUTH2_PROXY_SCOPE="openid email profile"
  export OAUTH2_PROXY_REVERSE_PROXY=true
  # Only the edge may set X-Forwarded-*.
  export OAUTH2_PROXY_TRUSTED_PROXY_IPS=127.0.0.1/32
  export OAUTH2_PROXY_SET_XAUTHREQUEST=true
  export OAUTH2_PROXY_SKIP_PROVIDER_BUTTON=true
  export OAUTH2_PROXY_CODE_CHALLENGE_METHOD=S256
  export OAUTH2_PROXY_UPSTREAMS=static://202
}
apps_proxy() {
  common
  export OAUTH2_PROXY_HTTP_ADDRESS=127.0.0.3:4180
  export OAUTH2_PROXY_COOKIE_DOMAINS=.apps.isp-insoft.de
  export OAUTH2_PROXY_WHITELIST_DOMAINS=.apps.isp-insoft.de
  export OAUTH2_PROXY_REDIRECT_URL=https://apps.isp-insoft.de/oauth2/callback
}
connect_proxy() {
  common
  export OAUTH2_PROXY_HTTP_ADDRESS=127.0.0.6:4180
  export OAUTH2_PROXY_COOKIE_NAME=__Host-traum-haft-connect
  export OAUTH2_PROXY_WHITELIST_DOMAINS=connect.apps.isp-insoft.de
  export OAUTH2_PROXY_REDIRECT_URL=https://connect.apps.isp-insoft.de/oauth2/callback
}
