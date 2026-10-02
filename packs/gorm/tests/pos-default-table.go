package model

type ChargingStation struct {
	StationID string `gorm:"primaryKey;column:station_id"`
	Watts     int
	Notes     string `gorm:"-"`
}
