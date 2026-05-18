from django.urls import path, re_path
from . import views

urlpatterns = [
    path("api/users/", views.user_list),
    path("api/users/<int:pk>/", views.user_detail),
    path("api/orders/", views.order_list),
    re_path(r"^api/search/(?P<query>.+)/$", views.search),
]
