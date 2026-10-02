const express = require("express");

const router = express.Router();

router.get("/v1/gizmos", (req, res) => res.json([]));
router.post("/v1/gizmos/:gizmoId/enable", function enableGizmo(req, res) {
  res.sendStatus(204);
});
router.all("/v1/ping", (req, res) => res.send("pong"));

module.exports = router;
