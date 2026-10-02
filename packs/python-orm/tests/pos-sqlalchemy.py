from sqlalchemy import Column, Integer, String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(DeclarativeBase):
    pass


class Gate(Base):
    __tablename__ = "gates"

    id: Mapped[str] = mapped_column(String(36), primary_key=True)
    code: Mapped[str] = mapped_column("gate_code", String(16))
    lanes = Column(Integer, default=1)
