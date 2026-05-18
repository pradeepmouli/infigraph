<?php
use Illuminate\Support\Facades\Route;
Route::get('/api/users', [UserController::class, 'index']);
Route::post('/api/users', [UserController::class, 'store']);
Route::get('/api/users/{id}', [UserController::class, 'show']);
Route::put('/api/users/{id}', [UserController::class, 'update']);
Route::delete('/api/users/{id}', [UserController::class, 'destroy']);
Route::resource('/api/orders', OrderController::class);
Route::group(['prefix' => '/api/admin'], function() {
    Route::get('/settings', [AdminController::class, 'settings']);
});
