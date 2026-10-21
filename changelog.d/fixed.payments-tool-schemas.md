- **The checkout tools declare their arguments** (`adk-payments`): `payments_checkout_create`,
  `payments_checkout_update`, `payments_checkout_complete`, and `payments_checkout_cancel`
  return a `parameters_schema` describing the arguments they parse (merchant, cart lines and
  `Money` amounts, transaction id, payment method). Without one each tool was declared to take
  no arguments, so a live model called it with `{}` and every call failed with
  `payments.tools.invalid_args`.
