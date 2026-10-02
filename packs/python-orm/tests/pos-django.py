from django.db import models


class Trailer(models.Model):
    plate = models.CharField(max_length=16)
    owner = models.ForeignKey("Carrier", on_delete=models.CASCADE)

    class Meta:
        db_table = "trailers"


class Carrier(models.Model):
    name = models.CharField(max_length=64)
