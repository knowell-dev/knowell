import { Column, Entity, PrimaryColumn } from "typeorm";

@Entity()
class LoadingSlot {
  @PrimaryColumn()
  slotId!: string;

  @Column({ type: "int" })
  minutes!: number;
}

@Entity("slot_notes")
export class SlotNote {
  @PrimaryColumn()
  id!: string;
}
