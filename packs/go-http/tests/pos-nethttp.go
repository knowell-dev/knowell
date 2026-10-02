package api

import "net/http"

func Register(mux *http.ServeMux, h *Handler) {
	mux.HandleFunc("GET /v1/parcels/{parcelID}", h.GetParcel)
	mux.HandleFunc("POST /v1/parcels", h.CreateParcel)
	mux.Handle("/healthz", http.HandlerFunc(h.Health))
	http.HandleFunc("/metrics", h.Metrics)
}
