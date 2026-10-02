//! `mobile-app`: the Flutter app for iOS and Android (Dart).

pub(super) const FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"
# mobile-app

Acme Goods for iOS and Android (Flutter). Members can browse, see their order
history and manage their Acme Plus subscription.

## Running

    flutter run \
      --dart-define=BILLING_API_URL=http://10.0.2.2:8080 \
      --dart-define=ORDERS_API_URL=http://10.0.2.2:8081

HTTP goes through one `dio` client per backend (`lib/api/api_client.dart`);
the login token is kept in the platform keychain (`lib/session/token_storage.dart`).

Strings are in `lib/l10n/app_en.arb` and `lib/l10n/app_tr.arb`; run
`flutter gen-l10n` after editing them.
"##,
    ),
    (
        "pubspec.yaml",
        r##"
name: acme_mobile
description: Acme Goods mobile app.
publish_to: none
version: 2.9.0+412

environment:
  sdk: ">=3.3.0 <4.0.0"

dependencies:
  flutter:
    sdk: flutter
  flutter_localizations:
    sdk: flutter
  dio: ^5.4.3
  flutter_secure_storage: ^9.2.2
  intl: ^0.19.0

dev_dependencies:
  flutter_test:
    sdk: flutter

flutter:
  generate: true
  uses-material-design: true
"##,
    ),
    (
        "lib/main.dart",
        r##"
import 'package:flutter/material.dart';
import 'package:flutter_gen/gen_l10n/app_localizations.dart';
import 'package:flutter_localizations/flutter_localizations.dart';

import 'api/api_client.dart';
import 'api/orders_api.dart';
import 'api/subscription_api.dart';
import 'screens/order_history_screen.dart';
import 'screens/subscription_screen.dart';
import 'session/token_storage.dart';

void main() {
  final tokens = TokenStorage();
  final subscriptions = SubscriptionApi(buildClient(billingApiUrl, tokens));
  final orders = OrdersApi(buildClient(ordersApiUrl, tokens));
  runApp(AcmeApp(subscriptions: subscriptions, orders: orders));
}

class AcmeApp extends StatelessWidget {
  const AcmeApp({super.key, required this.subscriptions, required this.orders});

  final SubscriptionApi subscriptions;
  final OrdersApi orders;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Acme Goods',
      localizationsDelegates: const [
        AppLocalizations.delegate,
        GlobalMaterialLocalizations.delegate,
        GlobalWidgetsLocalizations.delegate,
        GlobalCupertinoLocalizations.delegate,
      ],
      supportedLocales: const [Locale('en'), Locale('tr')],
      routes: {
        '/orders': (_) => OrderHistoryScreen(api: orders),
        '/membership': (_) => SubscriptionScreen(api: subscriptions),
      },
      initialRoute: '/orders',
    );
  }
}
"##,
    ),
    (
        "lib/api/api_client.dart",
        r##"
import 'package:dio/dio.dart';

import '../session/token_storage.dart';

/// Base URLs, injected with `--dart-define` at build time.
const billingApiUrl = String.fromEnvironment('BILLING_API_URL');
const ordersApiUrl = String.fromEnvironment('ORDERS_API_URL');

/// Builds a dio client that attaches the login token to every request and
/// forgets the token when the server answers 401.
Dio buildClient(String baseUrl, TokenStorage tokens) {
  final dio = Dio(BaseOptions(
    baseUrl: baseUrl,
    connectTimeout: const Duration(seconds: 10),
    receiveTimeout: const Duration(seconds: 20),
    contentType: 'application/json',
  ));
  dio.interceptors.add(InterceptorsWrapper(
    onRequest: (options, handler) async {
      final token = await tokens.read();
      if (token != null) {
        options.headers['Authorization'] = 'Bearer $token';
      }
      handler.next(options);
    },
    onError: (error, handler) async {
      if (error.response?.statusCode == 401) {
        await tokens.clear();
      }
      handler.next(error);
    },
  ));
  return dio;
}
"##,
    ),
    (
        "lib/api/subscription_api.dart",
        r##"
import 'package:dio/dio.dart';

import '../models/subscription.dart';

class SubscriptionApi {
  SubscriptionApi(this._dio);

  final Dio _dio;

  Future<Subscription> fetchSubscription(String subscriptionId) async {
    final response = await _dio.get<Map<String, dynamic>>('/v1/subscriptions/$subscriptionId');
    return Subscription.fromJson(response.data ?? const {});
  }

  /// Same endpoint as the website. The server keeps the benefits until the
  /// current period ends, so the returned status is `cancelled` but
  /// `currentPeriodEnd` is still in the future.
  Future<Subscription> cancelSubscription(
    String subscriptionId, {
    required String reason,
    String? feedback,
  }) async {
    final response = await _dio.post<Map<String, dynamic>>(
      '/v1/subscriptions/$subscriptionId/cancel',
      data: {'reason': reason, if (feedback != null) 'feedback': feedback},
    );
    return Subscription.fromJson(response.data ?? const {});
  }
}
"##,
    ),
    (
        "lib/api/orders_api.dart",
        r##"
import 'package:dio/dio.dart';

class OrderSummary {
  const OrderSummary({required this.id, required this.status, required this.totalMinor, required this.currency});

  final String id;
  final String status;
  final int totalMinor;
  final String currency;

  factory OrderSummary.fromJson(Map<String, dynamic> json) => OrderSummary(
        id: json['id'] as String,
        status: json['status'] as String,
        totalMinor: json['totalMinor'] as int,
        currency: json['currency'] as String,
      );
}

class OrdersApi {
  OrdersApi(this._dio);

  final Dio _dio;

  /// Pages through `GET /v1/orders` (served by orders-service).
  Future<(List<OrderSummary>, String?)> fetchOrders({String? cursor}) async {
    final response = await _dio.get<Map<String, dynamic>>(
      '/v1/orders',
      queryParameters: {if (cursor != null) 'cursor': cursor},
    );
    final body = response.data ?? const {};
    final items = (body['items'] as List<dynamic>? ?? const [])
        .map((item) => OrderSummary.fromJson(item as Map<String, dynamic>))
        .toList();
    return (items, body['nextCursor'] as String?);
  }
}
"##,
    ),
    (
        "lib/models/subscription.dart",
        r##"
class Subscription {
  const Subscription({
    required this.id,
    required this.status,
    required this.currentPeriodEnd,
    this.cancelledAt,
    this.cancelReason,
  });

  final String id;
  final String status;
  final DateTime currentPeriodEnd;
  final DateTime? cancelledAt;
  final String? cancelReason;

  bool get isCancelled => status == 'cancelled';

  factory Subscription.fromJson(Map<String, dynamic> json) => Subscription(
        id: json['id'] as String? ?? '',
        status: json['status'] as String? ?? 'active',
        currentPeriodEnd: DateTime.parse(json['currentPeriodEnd'] as String? ?? '1970-01-01T00:00:00Z'),
        cancelledAt: json['cancelledAt'] == null ? null : DateTime.parse(json['cancelledAt'] as String),
        cancelReason: json['cancelReason'] as String?,
      );
}
"##,
    ),
    (
        "lib/screens/subscription_screen.dart",
        r##"
import 'package:flutter/material.dart';
import 'package:flutter_gen/gen_l10n/app_localizations.dart';
import 'package:intl/intl.dart';

import '../api/subscription_api.dart';
import '../models/subscription.dart';

class SubscriptionScreen extends StatefulWidget {
  const SubscriptionScreen({super.key, required this.api, this.subscriptionId = 'current'});

  final SubscriptionApi api;
  final String subscriptionId;

  @override
  State<SubscriptionScreen> createState() => _SubscriptionScreenState();
}

class _SubscriptionScreenState extends State<SubscriptionScreen> {
  Subscription? _subscription;
  bool _busy = false;

  @override
  void initState() {
    super.initState();
    widget.api.fetchSubscription(widget.subscriptionId).then((value) {
      if (mounted) setState(() => _subscription = value);
    });
  }

  Future<void> _onCancelPressed() async {
    final l10n = AppLocalizations.of(context);
    // Kullanıcı onay vermeden iptal isteği gönderilmez.
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(l10n.cancelDialogTitle),
        content: Text(l10n.cancelDialogBody),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: Text(l10n.keepMembership)),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: Text(l10n.confirmCancel)),
        ],
      ),
    );
    if (confirmed != true) return;
    setState(() => _busy = true);
    try {
      final updated = await widget.api.cancelSubscription(widget.subscriptionId, reason: 'not_using');
      if (mounted) setState(() => _subscription = updated);
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final l10n = AppLocalizations.of(context);
    final subscription = _subscription;
    if (subscription == null) {
      return const Scaffold(body: Center(child: CircularProgressIndicator()));
    }
    final until = DateFormat.yMMMMd(Localizations.localeOf(context).toString()).format(subscription.currentPeriodEnd);
    return Scaffold(
      appBar: AppBar(title: Text(l10n.subscriptionTitle)),
      body: Padding(
        padding: const EdgeInsets.all(16),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(subscription.isCancelled ? l10n.activeUntil(until) : l10n.renewsOn(until)),
            const SizedBox(height: 24),
            if (!subscription.isCancelled)
              OutlinedButton(
                onPressed: _busy ? null : _onCancelPressed,
                child: Text(l10n.cancelSubscriptionButton),
              ),
          ],
        ),
      ),
    );
  }
}
"##,
    ),
    (
        "lib/screens/order_history_screen.dart",
        r##"
import 'package:flutter/material.dart';
import 'package:flutter_gen/gen_l10n/app_localizations.dart';

import '../api/orders_api.dart';

class OrderHistoryScreen extends StatefulWidget {
  const OrderHistoryScreen({super.key, required this.api});

  final OrdersApi api;

  @override
  State<OrderHistoryScreen> createState() => _OrderHistoryScreenState();
}

class _OrderHistoryScreenState extends State<OrderHistoryScreen> {
  final List<OrderSummary> _orders = [];
  String? _cursor;
  bool _loading = false;

  @override
  void initState() {
    super.initState();
    _loadMore();
  }

  Future<void> _loadMore() async {
    if (_loading) return;
    setState(() => _loading = true);
    final (items, next) = await widget.api.fetchOrders(cursor: _cursor);
    if (!mounted) return;
    setState(() {
      _orders.addAll(items);
      _cursor = next;
      _loading = false;
    });
  }

  @override
  Widget build(BuildContext context) {
    final l10n = AppLocalizations.of(context);
    return Scaffold(
      appBar: AppBar(title: Text(l10n.orderHistoryTitle)),
      body: ListView.builder(
        itemCount: _orders.length + (_cursor == null ? 0 : 1),
        itemBuilder: (context, index) {
          if (index >= _orders.length) {
            _loadMore();
            return const Center(child: CircularProgressIndicator());
          }
          final order = _orders[index];
          return ListTile(title: Text(order.id), subtitle: Text(order.status));
        },
      ),
    );
  }
}
"##,
    ),
    (
        "lib/session/token_storage.dart",
        r##"
import 'package:flutter_secure_storage/flutter_secure_storage.dart';

/// Keeps the login token in the iOS keychain / Android keystore instead of
/// shared preferences.
class TokenStorage {
  TokenStorage([FlutterSecureStorage? storage]) : _storage = storage ?? const FlutterSecureStorage();

  static const _key = 'acme.session';
  final FlutterSecureStorage _storage;

  Future<String?> read() => _storage.read(key: _key);

  Future<void> write(String token) => _storage.write(key: _key, value: token);

  Future<void> clear() => _storage.delete(key: _key);
}
"##,
    ),
    (
        "lib/l10n/app_en.arb",
        r##"
{
  "@@locale": "en",
  "subscriptionTitle": "Acme Plus",
  "cancelSubscriptionButton": "Cancel subscription",
  "cancelDialogTitle": "Cancel your subscription?",
  "cancelDialogBody": "Your benefits stay active until the end of the billing period.",
  "keepMembership": "Keep it",
  "confirmCancel": "Cancel subscription",
  "orderHistoryTitle": "Order history",
  "renewsOn": "Renews on {date}",
  "@renewsOn": {
    "placeholders": {
      "date": { "type": "String" }
    }
  },
  "activeUntil": "Cancelled, active until {date}",
  "@activeUntil": {
    "placeholders": {
      "date": { "type": "String" }
    }
  }
}
"##,
    ),
    (
        "lib/l10n/app_tr.arb",
        r##"
{
  "@@locale": "tr",
  "subscriptionTitle": "Acme Plus",
  "cancelSubscriptionButton": "Aboneliği iptal et",
  "cancelDialogTitle": "Aboneliğiniz iptal edilsin mi?",
  "cancelDialogBody": "Avantajlarınız fatura döneminin sonuna kadar devam eder.",
  "keepMembership": "Vazgeç",
  "confirmCancel": "Aboneliği iptal et",
  "orderHistoryTitle": "Sipariş geçmişi",
  "renewsOn": "{date} tarihinde yenilenir",
  "activeUntil": "İptal edildi, {date} tarihine kadar aktif"
}
"##,
    ),
];
