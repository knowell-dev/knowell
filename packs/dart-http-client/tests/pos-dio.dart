import 'package:dio/dio.dart';

class LockerApi {
  LockerApi(this._dio);

  final Dio _dio;

  Future<void> open(String lockerId) async {
    await _dio.post('/v1/lockers/$lockerId/open', data: {'force': false});
  }

  Future<Response<dynamic>> list() => _dio.get('/v1/lockers');

  Future<void> remove(String lockerId) => _dio.delete('/v1/lockers/${lockerId}');
}
