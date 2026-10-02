class RouteNames {
  static const orders = '/v1/orders';

  String label(Map<String, String> labels) => labels['/v1/orders'] ?? 'Orders';

  String first(List<String> items) => items.firstWhere((item) => item == '/v1/orders');
}
