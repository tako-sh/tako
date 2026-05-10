package internal

import (
	"encoding/json"
	"net/http"
	"os"
	"strings"
	"time"
)

const internalTokenHeader = "x-tako-internal-token"
const internalHostSuffix = ".tako"
const internalChannelAuthorizePath = "/channels/authorize"
const internalChannelRegistryPath = "/channels/registry"

// StatusResponse is the JSON shape returned by GET /status on Host: <app>.tako.
type StatusResponse struct {
	Status        string `json:"status"`
	InstanceID    string `json:"instance_id"`
	Version       string `json:"version"`
	PID           int    `json:"pid"`
	UptimeSeconds int64  `json:"uptime_seconds"`
}

// EndpointHandler intercepts Host: <app>.tako requests for internal endpoints.
type EndpointHandler struct {
	appName       string
	startTime     time.Time
	instanceID    string
	version       string
	internalToken string
	userApp       http.Handler
}

// NewEndpointHandler creates a handler that intercepts Tako internal requests.
func NewEndpointHandler(appName, instanceID, version, internalToken string, userApp http.Handler) *EndpointHandler {
	return &EndpointHandler{
		appName:       appName,
		startTime:     time.Now(),
		instanceID:    instanceID,
		version:       version,
		internalToken: internalToken,
		userApp:       userApp,
	}
}

// ServeHTTP dispatches to internal endpoints or the user's app.
func (h *EndpointHandler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if normalizeHost(r.Host) == h.internalHost() {
		h.handleInternal(w, r)
		return
	}
	h.userApp.ServeHTTP(w, r)
}

func (h *EndpointHandler) handleInternal(w http.ResponseWriter, r *http.Request) {
	// Verify internal token when set (production mode).
	// In dev mode (no token), all Host:<app>.tako requests are allowed.
	if h.internalToken != "" {
		if r.Header.Get(internalTokenHeader) != h.internalToken {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
	}

	switch {
	case r.Method == http.MethodGet && r.URL.Path == "/status":
		h.handleStatus(w)
	case r.Method == http.MethodPost && r.URL.Path == internalChannelAuthorizePath:
		h.handleChannelAuthorize(w, r)
	case r.Method == http.MethodGet && r.URL.Path == internalChannelRegistryPath:
		h.handleChannelRegistry(w)
	default:
		http.NotFound(w, r)
	}
}

func (h *EndpointHandler) handleStatus(w http.ResponseWriter) {
	resp := StatusResponse{
		Status:        "healthy",
		InstanceID:    h.instanceID,
		Version:       h.version,
		PID:           os.Getpid(),
		UptimeSeconds: int64(time.Since(h.startTime).Seconds()),
	}
	w.Header().Set("Content-Type", "application/json")
	if h.internalToken != "" {
		w.Header().Set(internalTokenHeader, h.internalToken)
	}
	json.NewEncoder(w).Encode(resp)
}

func (h *EndpointHandler) handleChannelAuthorize(w http.ResponseWriter, r *http.Request) {
	var input ChannelAuthorizeInput
	if err := json.NewDecoder(r.Body).Decode(&input); err != nil {
		http.Error(w, "invalid json", http.StatusBadRequest)
		return
	}
	if input.Channel == "" || input.Operation == "" {
		http.Error(w, "invalid request", http.StatusBadRequest)
		return
	}

	response, defined, allowed := Channels.Authorize(input)
	if !defined {
		http.Error(w, "channel not defined", http.StatusNotFound)
		return
	}
	if !allowed {
		http.Error(w, "forbidden", http.StatusForbidden)
		return
	}

	w.Header().Set("Content-Type", "application/json")
	if h.internalToken != "" {
		w.Header().Set(internalTokenHeader, h.internalToken)
	}
	json.NewEncoder(w).Encode(response)
}

func (h *EndpointHandler) handleChannelRegistry(w http.ResponseWriter) {
	w.Header().Set("Content-Type", "application/json")
	if h.internalToken != "" {
		w.Header().Set(internalTokenHeader, h.internalToken)
	}
	json.NewEncoder(w).Encode(Channels.Metadata())
}

func normalizeHost(host string) string {
	host = strings.TrimSpace(strings.ToLower(host))
	if host == "" {
		return ""
	}
	parts := strings.Split(host, ":")
	return parts[0]
}

func (h *EndpointHandler) internalHost() string {
	appName := strings.TrimSpace(strings.ToLower(h.appName))
	if appName == "" {
		appName = "app"
	} else if baseAppName, _, ok := strings.Cut(appName, "/"); ok {
		appName = baseAppName
		if appName == "" {
			appName = "app"
		}
	}
	return appName + internalHostSuffix
}
