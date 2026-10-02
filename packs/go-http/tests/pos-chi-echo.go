package api

import (
	"github.com/go-chi/chi/v5"
	"github.com/labstack/echo/v4"
)

func ChiRoutes(r chi.Router, h *Handler) {
	r.Get("/v1/bins/{binID}", h.GetBin)
	r.Post("/v1/bins", h.CreateBin)
}

func EchoRoutes(e *echo.Echo, h *Handler) {
	e.PUT("/v1/slots/:slot", h.PutSlot)
}
