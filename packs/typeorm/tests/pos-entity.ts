import { Column, CreateDateColumn, Entity, PrimaryGeneratedColumn } from "typeorm";

@Entity({ name: "docks" })
export class DockEntity {
  @PrimaryGeneratedColumn("uuid")
  id!: string;

  @Column({ name: "dock_code", type: "varchar" })
  code!: string;

  @Column()
  capacity!: number;

  @CreateDateColumn({ name: "created_at" })
  createdAt!: Date;

  // Not a column.
  label?: string;
}
