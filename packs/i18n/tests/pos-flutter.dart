import 'package:flutter/material.dart';
import 'package:flutter_gen/gen_l10n/app_localizations.dart';

class GateTitle extends StatelessWidget {
  const GateTitle({super.key});

  @override
  Widget build(BuildContext context) {
    final l10n = AppLocalizations.of(context)!;
    return Column(children: [
      Text(AppLocalizations.of(context)!.gateTitle),
      Text(l10n.gateSubtitle),
      Text(l10n.openUntil('18:00')),
    ]);
  }
}
