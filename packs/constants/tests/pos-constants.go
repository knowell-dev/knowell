package events

const DockTopic = "dock.updated"

const (
	DockQueue = "dock.queue"
	retries   = 3
)

var dockSubjects = []string{"dock.x", "dock.y"}
