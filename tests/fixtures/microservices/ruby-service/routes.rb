Rails.application.routes.draw do
  get "/api/users", to: "users#index"
  post "/api/users", to: "users#create"
  get "/api/users/:id", to: "users#show"
  put "/api/users/:id", to: "users#update"
  delete "/api/users/:id", to: "users#destroy"
  resources :orders
  namespace :admin do
    resources :settings
  end
end
