package api

import "github.com/gin-gonic/gin"

func Routes(r *gin.Engine, h *Handler) {
	r.GET("/v1/crates/:id", h.GetCrate)
	v2 := r.Group("/v2")
	v2.POST("/crates", h.CreateCrate)
	v2.DELETE("/crates/:id", h.DeleteCrate)
}
