export default {
  computed: {
    title(): string {
      return this.$t("yard.title");
    },
  },
};

export const label = (i18n: { t(key: string): string }) => i18n.t("yard.label");
