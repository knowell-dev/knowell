import { useTranslation } from "react-i18next";

export function DockBanner({ status }: { status: string }) {
  const { t } = useTranslation();
  return (
    <div>
      <h1>{t("dock.banner.title")}</h1>
      <p>{t(`dock.status.${status}`)}</p>
      <span>{i18n.t("dock.banner.subtitle", { count: 2 })}</span>
    </div>
  );
}
