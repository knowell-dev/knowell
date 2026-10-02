package model

import "time"

type Forklift struct {
	ID        string    `gorm:"column:id;primaryKey"`
	Serial    string    `gorm:"column:serial_no"`
	MaxLoad   int       `gorm:"not null"`
	UpdatedAt time.Time `json:"updatedAt"`
}

func (Forklift) TableName() string {
	return "forklifts"
}
